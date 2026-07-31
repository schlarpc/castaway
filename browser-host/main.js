// castaway's browser, as a subprocess (D36).
//
// Dependency-free by policy: Electron APIs only, no npm tree. That is what keeps "a Node
// runtime joined a Rust appliance" to a runtime rather than an ecosystem, and it is a
// condition of the decision that let JavaScript into this repo at all.
//
// This process owns no policy. It renders, reports, and asks — blocking decisions, page
// choice and scriptlet content all come from castaway over the control socket. Everything here is
// mechanism, so that the parts worth testing stay in Rust where they can be
// fixture-tested (`pipeline::browser_proto`).
//
// The protocol is documented in crates/pipeline/src/browser_proto.rs. Two rules matter
// on this side:
//
//   1. A painted frame is *lent*, not given. `texture.release()` happens when castaway
//      says `release`, never before — it is still sampling. Frames beyond MAX_INFLIGHT
//      are dropped rather than queued, because for live output latency beats freshness.
//   2. The control channel is a local socket, not stdio — see the `SOCKET` comment below
//      for why Windows forced that. stdout and stderr are diagnostics, inherited by
//      castaway, and nothing on them can desynchronize framing.

'use strict';

const { app, BrowserWindow, components, dialog, session } = require('electron');
const fs = require('fs');
const net = require('net');
const path = require('path');

// Injected into every page's main world: routes the page's audio to castaway instead of
// to the sound card. See audio-tap.js for why this cannot be done from the host side.
const AUDIO_TAP = fs.readFileSync(path.join(__dirname, 'audio-tap.js'), 'utf8');

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
// Mirrors MAX_INFLIGHT_FRAMES in crates/pipeline/src/browser_proto.rs — its docs say
// why four is the number. Enforced per *window*: each surface has its own pending slot
// on castaway's side, so a 60 fps page must not be able to spend the clock's budget.
const MAX_INFLIGHT = 4;

// The control channel. Not stdio: Electron's main-process `process.stdin` is unusable on
// Windows — GUI-subsystem startup loses the piped handle, so it emits `end` immediately
// and never delivers a byte (electron#4218, #10580, #11680, #22809). castaway listens on a
// local socket before spawning us and names it here; `net.connect` takes a Unix socket path
// and a `\\.\pipe\` name identically, so one spelling serves both platforms.
//
// stdout is no longer the control channel, which is a relief rather than a cost: it was
// shared with everything Chromium decides to print, and on Windows that includes CRLF and
// a stray leading blank line (electron#12578). Diagnostics can have it back.
const SOCKET = process.env.CASTAWAY_BROWSER_SOCKET;
if (!SOCKET) {
  // Nothing to report the failure *over*, so this is the one place stderr is the channel.
  process.stderr.write('castaway: CASTAWAY_BROWSER_SOCKET is unset; nothing to talk to\n');
  process.exit(2);
}
// Node queues writes issued before the connection completes, so `send` is usable
// immediately — which matters, because staging the CDM logs before the socket is up.
const wire = net.connect(SOCKET);
wire.setNoDelay(true);
wire.on('error', (e) => {
  process.stderr.write(`castaway: control socket ${SOCKET}: ${e}\n`);
  app.exit(1);
});

function send(msg) {
  wire.write(JSON.stringify(msg) + '\n');
}
function log(level, message) {
  send({ type: 'log', level, message });
}

/** Frames lent to castaway, id → { texture, win }. Ids are unique across windows, so a
 * release does not need to say which window it returns to — but the entry remembers,
 * because the per-window lent count has to come back down on the right window. */
const inflight = new Map();
let paintSeq = 0;
let drops = 0;

/** Pending adblock questions, id → resolve. */
const pendingBlock = new Map();
let blockSeq = 0;

/**
 * surface → BrowserWindow, per crates/pipeline/src/browser_proto.rs `Surface`: 'widget'
 * is the idle clock, created when castaway first navigates it; 'page' is the cast
 * surface, created on its first navigate. Two windows so the two have separate
 * webContents — separate navigation state, separate renderers — and opening a cast never
 * flashes the clock through it. If a second window cannot be created, both surfaces
 * share the survivor (logged): degraded single-window behaviour, not a crash.
 *
 * Per-window state lives on the window itself (`__surface`, `__touchPoints`, `__lent`,
 * `__scriptletHandle`) so shared-fallback mode cannot cross wires between two maps.
 */
const wins = new Map();

function getWindow(surface) {
  const w = wins.get(surface);
  return w && !w.isDestroyed() ? w : null;
}

/** Pending scriptlet questions, id → resolve. */
const pendingScriptlets = new Map();
let scriptletSeq = 0;

const USER_AGENT = process.env.CASTAWAY_USER_AGENT || undefined;

// ---------------------------------------------------------------------------
// Widevine. Two halves: pre-staging the pinned CDM into the profile, then waiting for
// ECS to pick it up. See #66 for the measurement behind both.
// ---------------------------------------------------------------------------

/**
 * The Windows shared-texture handle, whatever Electron decided to call it this release.
 *
 * The docs describe `sharedTextureHandle`, a number. Electron 43 on Windows actually
 * delivers `ntHandle`: an 8-byte little-endian Buffer holding the HANDLE. Reading only the
 * documented name meant every paint on the panel was rejected with "handle carries neither
 * nativePixmap nor sharedTextureHandle" and the browser layer stayed empty — the browser
 * ran, painted, and had all of its frames thrown away at this line.
 *
 * Both spellings are accepted rather than switching on version, because the cost of the
 * wrong guess here is a black panel with a message that sounds like a GPU problem.
 *
 * @returns {number|undefined} the handle value, or undefined if this is not a Windows
 *   shared texture.
 */
function sharedHandleValue(handle) {
  if (handle.sharedTextureHandle !== undefined && handle.sharedTextureHandle !== null) {
    return Number(handle.sharedTextureHandle);
  }
  const nt = handle.ntHandle;
  if (nt === undefined || nt === null) return undefined;
  // A Buffer over the protocol boundary, or a plain number if a future release simplifies.
  if (typeof nt === 'number') return nt;
  const buf = Buffer.isBuffer(nt) ? nt : Buffer.from(nt.data || nt);
  if (buf.length < 8) return undefined;
  // HANDLE is pointer-sized, but the values Windows hands out are small enough that the
  // BigInt lands well inside Number's exact-integer range.
  return Number(buf.readBigUInt64LE(0));
}

/** `_platform_specific` leaf name, in ECS's spelling. */
function cdmPlatform() {
  const os = { win32: 'win', darwin: 'mac', linux: 'linux' }[process.platform] || process.platform;
  return `${os}_${process.arch === 'x64' ? 'x64' : process.arch}`;
}

/**
 * Copy the pinned CDM into the profile, in the layout ECS's own component updater writes.
 *
 * This is G46's property — a panel that has never been online can still play protected
 * video — and it is a *startup* step rather than something the packaging can do, because
 * the marker file holds an **absolute** path to the profile, which is not known until the
 * receiver is running. That is why it lives here and not in Nix.
 *
 * It used to live in `stage-widevine.sh`, which meant it only ever ran when a human ran
 * it: nothing invoked the script, and the `CASTAWAY_WIDEVINE_CDM` the Linux wrapper has
 * always set had no reader. On Windows there is not even a shell to run it with. So the
 * offline-DRM property was real when measured and dead in every shipped artifact.
 *
 * Synchronous, and at module load rather than inside `readyComponents()`: `components`
 * begins its own work as the app starts, so staging has to be finished before it looks.
 * The cost is a ~6 MB copy on first boot only — afterwards the marker matches and this
 * returns without touching the disk.
 *
 * Every failure here is a warning, never a throw. A receiver that cannot stage a CDM
 * should come up without DRM, not fail to come up (G31).
 */
function stageWidevine() {
  // The Linux wrapper points at the store path; the Windows artifact stages `WidevineCdm/`
  // beside castaway.exe, which is `browser-host/`'s parent.
  const src = process.env.CASTAWAY_WIDEVINE_CDM || path.join(__dirname, '..', 'WidevineCdm');
  const manifest = path.join(src, 'manifest.json');
  if (!fs.existsSync(manifest)) {
    log('info', `widevine: no CDM to stage at ${src}; ECS will fetch one if it can`);
    return;
  }
  const version = JSON.parse(fs.readFileSync(manifest, 'utf8')).version;
  if (!version) {
    log('warn', `widevine: no version in ${manifest}; not staging`);
    return;
  }

  const plat = cdmPlatform();
  const libDir = path.join(src, '_platform_specific', plat);
  const lib = ['libwidevinecdm.so', 'widevinecdm.dll', 'libwidevinecdm.dylib']
    .map((n) => path.join(libDir, n))
    .find((p) => fs.existsSync(p));
  if (!lib) {
    log('warn', `widevine: no CDM binary for ${plat} under ${src}; not staging`);
    return;
  }

  const root = path.join(app.getPath('userData'), 'WidevineCdm');
  const dest = path.join(root, version);
  const marker = path.join(root, 'latest-component-updated-widevine-cdm');
  const staged = path.join(dest, '_platform_specific', plat, path.basename(lib));
  if (fs.existsSync(staged) && fs.existsSync(marker)) {
    return;
  }

  fs.mkdirSync(path.dirname(staged), { recursive: true });
  fs.copyFileSync(manifest, path.join(dest, 'manifest.json'));
  fs.copyFileSync(lib, staged);
  // Absolute, because that is what the component updater writes and what Chromium reads
  // back. A relative path here silently yields no CDM.
  fs.writeFileSync(marker, JSON.stringify({ Path: dest }));
  log('info', `widevine: staged ${version} for ${plat} into ${root}`);
}

try {
  stageWidevine();
} catch (e) {
  log('warn', `widevine: staging failed (${e}); protected playback will fail, the rest is unaffected`);
}

// Blocking here would be a hang on a path castaway awaits, so it is raced against a
// deadline: a pre-staged CDM makes this resolve in milliseconds, and a box with no route
// to Google gets a receiver that starts without DRM rather than one that never starts.
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
      source: sourceUrlFor(details),
      kind: details.resourceType || 'other',
    });
  });
}

/** The document URL a request came from, for domain-scoped rules. With two windows the
 * requester matters: a rule scoped to youtube.com must not fire on the clock's requests,
 * so the window is found by the request's own webContents rather than assumed. */
function sourceUrlFor(details) {
  for (const w of new Set(wins.values())) {
    if (w && !w.isDestroyed() && w.webContents.id === details.webContentsId) {
      return w.webContents.getURL();
    }
  }
  // A request with no window of ours (a service worker, say): the page window's
  // document is the best available answer, as it was when there was only one window.
  const page = getWindow('page') || getWindow('widget');
  return (page && page.webContents.getURL()) || '';
}

// ---------------------------------------------------------------------------
// Scriptlets. uBlock Origin's `##+js(...)` bodies must run in the page's **main world**
// and before any page script, because they patch globals the page then uses. A preload
// script cannot do it: under contextIsolation it runs in an isolated world, where the
// patches are invisible to the page, and turning contextIsolation off would trade the
// renderer sandbox for it. CDP's addScriptToEvaluateOnNewDocument is main-world and
// document-start, which is exactly the pair required.
// ---------------------------------------------------------------------------
/// Every CDP command gets a deadline.
///
/// Not defensive decoration: a command sent to a webContents with no target never
/// answers, and one unresolved await in the navigate chain is a page that never loads.
/// That failure has no error and no log line — the panel is simply black.
function withDeadline(promise, what, ms = 3000) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} did not answer in ${ms}ms`)), ms);
    }),
  ]).finally(() => clearTimeout(timer));
}

// The debugger is attached once per window and kept, because *three* things need it:
// scriptlet injection, the audio tap's binding, and touch. An earlier version attached it
// lazily inside the scriptlet path, which meant a page with no matching uBO rule got no
// debugger — and therefore no touch and no audio. That failed silently, on exactly the
// pages least likely to be noticed, which is why `a_touch_reaches_the_page` exists.
async function ensureDebugger(w) {
  const contents = w.webContents;
  if (contents.debugger.isAttached()) return true;
  try {
    contents.debugger.attach('1.3');
  } catch (e) {
    log('warn', `debugger attach failed; touch, audio and scriptlets are all unavailable: ${e}`);
    return false;
  }
  try {
    // `Page` is what addScriptToEvaluateOnNewDocument lives on; without enabling it the
    // call answers "No target available" even when a target exists.
    await withDeadline(contents.debugger.sendCommand('Page.enable'), 'Page.enable');
    await withDeadline(contents.debugger.sendCommand('Runtime.enable'), 'Runtime.enable');
    await withDeadline(
      contents.debugger.sendCommand('Runtime.addBinding', { name: '__castawayAudio' }),
      'addBinding audio'
    );
    await withDeadline(
      contents.debugger.sendCommand('Runtime.addBinding', { name: '__castawayAudioError' }),
      'addBinding audio-error'
    );
    contents.debugger.on('message', (_e, method, params) => {
      if (method !== 'Runtime.bindingCalled') return;
      if (params.name === '__castawayAudioError') {
        log('warn', `audio tap: ${params.payload}`);
        return;
      }
      if (params.name !== '__castawayAudio') return;
      try {
        const b = JSON.parse(params.payload);
        send({
          type: 'audio',
          // Read at delivery time, not attach time: in shared-window fallback the
          // window's surface changes with each navigate.
          surface: w.__surface,
          pcm: b.pcm,
          channels: b.channels,
          sampleRate: b.sampleRate,
          mediaTime: b.mediaTime,
          paused: b.paused,
        });
      } catch (e) {
        log('warn', `audio block: ${e}`);
      }
    });
  } catch (e) {
    log('warn', `debugger setup incomplete: ${e}`);
  }
  return true;
}

// ---------------------------------------------------------------------------
// Scriptlets. uBlock Origin's `##+js(...)` bodies must run in the page's **main world**
// and before any page script, because they patch globals the page then uses. A preload
// script cannot do it: under contextIsolation it runs in an isolated world, where the
// patches are invisible to the page, and turning contextIsolation off would trade the
// renderer sandbox for it. CDP's addScriptToEvaluateOnNewDocument is main-world and
// document-start, which is exactly the pair required.
//
// The audio tap rides along because it needs the same two properties.
// ---------------------------------------------------------------------------
async function applyScriptlets(w, source) {
  const contents = w.webContents;
  if (!(await ensureDebugger(w))) return;
  try {
    // The registration handle is per-window: each webContents keeps its own injected
    // blob, and replacing the page's must not strip the clock's.
    if (w.__scriptletHandle) {
      await withDeadline(
        contents.debugger.sendCommand('Page.removeScriptToEvaluateOnNewDocument', {
          identifier: w.__scriptletHandle,
        }),
        'removeScriptToEvaluateOnNewDocument'
      ).catch(() => {});
    }
    const res = await withDeadline(
      contents.debugger.sendCommand('Page.addScriptToEvaluateOnNewDocument', {
        source: `${AUDIO_TAP}\n${source || ''}`,
        runImmediately: true,
      }),
      'addScriptToEvaluateOnNewDocument'
    );
    w.__scriptletHandle = res.identifier;
    log('info', `${w.__surface} armed (audio tap + ${(source || '').length} bytes of scriptlets)`);
  } catch (e) {
    log('warn', `scriptlet injection failed: ${e}`);
  }
}

/// Ask castaway what to inject for `url`, with a deadline.
///
/// Per-navigation because uBO rules are domain-scoped. A late answer must not hold the
/// page: an un-patched page is a far better outcome than one that never loads.
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
    setTimeout(() => finish(''), 2000);
    send({ type: 'scriptlet-query', id, url });
  });
}

// ---------------------------------------------------------------------------
// Input. `webContents.sendInputEvent` has no touch type at all, so touch goes through
// CDP too. Chromium's own gesture recognizer is on the far side of both, so scroll,
// fling and tap behave as they did embedded.
// ---------------------------------------------------------------------------
async function dispatchTouch(msg) {
  const win = getWindow(msg.surface);
  if (!win) return;
  const contents = win.webContents;
  if (!contents.debugger.isAttached()) return;

  const type =
    msg.phase === 'start' ? 'touchStart'
    : msg.phase === 'move' ? 'touchMove'
    : msg.phase === 'cancel' ? 'touchCancel'
    : 'touchEnd';

  // Contacts are per-window: a finger down on the page must not appear in the
  // widget's touch list, or lifting it there would leave the page a phantom finger.
  const touchPoints = win.__touchPoints;
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
  const win = getWindow(msg.surface);
  if (!win) return;
  const contents = win.webContents;
  const type = msg.kind === 'down' ? 'mouseDown' : msg.kind === 'up' ? 'mouseUp' : 'mouseMove';
  contents.sendInputEvent({ type, x: Math.round(msg.x), y: Math.round(msg.y), button: 'left', clickCount: 1 });
}

// ---------------------------------------------------------------------------
// The windows: one per surface, created lazily on that surface's first navigate.
// ---------------------------------------------------------------------------
/** The window a navigate should act on: the surface's own, created now if need be. If
 * creation fails and another window exists, both surfaces share it — single-window
 * behaviour as a logged fallback, because a receiver with a clock through its cast page
 * is degraded and a receiver that crashed is gone. */
function ensureWindow(surface, width, height) {
  const existing = getWindow(surface);
  if (existing) {
    existing.setContentSize(width, height);
    return existing;
  }
  try {
    const w = createWindow(surface, width, height);
    wins.set(surface, w);
    return w;
  } catch (e) {
    const survivor = [...new Set(wins.values())].find((o) => o && !o.isDestroyed());
    if (!survivor) throw e;
    log(
      'warn',
      `creating the ${surface} window failed (${e}); sharing one window for both surfaces`
    );
    wins.set(surface, survivor);
    survivor.setContentSize(width, height);
    return survivor;
  }
}

function createWindow(surface, width, height) {
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
  // Per-window state, kept on the window so shared-fallback mode cannot mismatch a
  // window with some other window's bookkeeping. `__surface` is the tag every outbound
  // message about this window carries; navigate re-stamps it, which only matters when
  // one window is serving both surfaces.
  w.__surface = surface;
  w.__touchPoints = new Map();
  w.__lent = 0;
  w.__scriptletHandle = null;
  // Constructor sizes are clamped to the display work area even offscreen — asking for
  // 4K silently yields the panel's size instead. setContentSize is not clamped.
  w.setContentSize(width, height);
  if (USER_AGENT) w.webContents.setUserAgent(USER_AGENT);
  w.webContents.setFrameRate(60);
  // Audio plays through the system device, mixed by the OS with castaway's own output.
  // Electron exposes no PCM tap the way CEF's audio handler did, so this is the honest
  // arrangement rather than a chosen one; see GAPS for what it costs.
  w.webContents.setAudioMuted(false);
  // A window with nothing loaded has no CDP *target*, and commands sent to it never
  // answer — they do not fail, they hang, which took the whole navigate chain down with
  // them and left the panel black with no error. about:blank is the cheapest way to give
  // the debugger something to attach to.
  w.loadURL('about:blank').catch(() => {});

  w.webContents.on('paint', (event) => {
    if (!event.texture) {
      send({ type: 'no-texture', detail: 'paint arrived without a shared texture' });
      return;
    }
    const info = event.texture.textureInfo;
    const handle = info.handle || {};
    const pixmap = handle.nativePixmap;
    const shared = sharedHandleValue(handle);
    if (!pixmap && shared === undefined) {
      send({ type: 'no-texture', detail: `handle carries no usable texture: ${JSON.stringify(info.handle)}` });
      event.texture.release();
      return;
    }
    if (w.__lent >= MAX_INFLIGHT) {
      drops += 1;
      send({ type: 'dropped', total: drops });
      event.texture.release();
      return;
    }
    const id = ++paintSeq;
    w.__lent += 1;
    inflight.set(id, { texture: event.texture, win: w });
    send({
      type: 'paint',
      surface: w.__surface,
      id,
      format: info.pixelFormat === 'rgba' ? 'rgba' : 'bgra',
      width: info.codedSize.width,
      height: info.codedSize.height,
      mediaTime: info.timestamp ? info.timestamp / 1e6 : 0,
      // Linux: per-plane fds plus a DRM modifier. Windows: one NT handle and no
      // modifier, because the handle describes its own layout.
      modifier: pixmap ? String(pixmap.modifier) : null,
      planes: pixmap
        ? pixmap.planes.map((p) => ({ fd: p.fd, stride: p.stride, offset: p.offset }))
        : [{ fd: Number(shared), stride: 0, offset: 0 }],
    });
  });

  w.webContents.on('did-finish-load', () => {
    send({ type: 'load-end', surface: w.__surface, url: w.webContents.getURL(), status: null });
  });
  w.webContents.on('did-fail-load', (_e, code, desc, url) => {
    // -3 is ERR_ABORTED, which a navigation that superseded another produces routinely.
    if (code === -3) return;
    send({ type: 'load-error', surface: w.__surface, url: url || '', error: `${desc} (${code})` });
  });
  w.webContents.on('render-process-gone', (_e, details) => {
    send({ type: 'render-gone', surface: w.__surface, reason: (details && details.reason) || 'unknown' });
  });

  return w;
}

// ---------------------------------------------------------------------------
// Commands from castaway
// ---------------------------------------------------------------------------
function handle(msg) {
  switch (msg.type) {
    case 'release': {
      const lent = inflight.get(msg.id);
      if (lent) {
        inflight.delete(msg.id);
        if (lent.win && !lent.win.isDestroyed()) lent.win.__lent -= 1;
        lent.texture.release();
      }
      return;
    }
    case 'navigate': {
      const win = ensureWindow(msg.surface, msg.width, msg.height);
      // Re-stamped on every navigate: a no-op with two windows, and the thing that
      // keeps paints truthfully tagged when one window is serving both surfaces.
      win.__surface = msg.surface;
      // Ask for this page's scriptlets and arm them *before* navigating: uBO rules are
      // domain-scoped, and arming after the load has already begun races the very page
      // scripts the patches exist to get in front of.
      requestScriptlets(msg.url)
        .then((source) => applyScriptlets(win, source))
        .catch((e) => log('warn', `scriptlets for ${msg.url}: ${e}`))
        .finally(() =>
          win.loadURL(msg.url).catch((e) =>
            send({ type: 'load-error', surface: win.__surface, url: msg.url, error: String(e) })
          )
        );
      return;
    }
    case 'blank': {
      const win = getWindow(msg.surface);
      if (win) win.loadURL('about:blank').catch(() => {});
      return;
    }
    case 'resize': {
      const win = getWindow(msg.surface);
      if (win) {
        win.setContentSize(msg.width, msg.height);
        // Demand a paint at the new size — even when the size did not change, because
        // castaway also resizes a window it just put back on the glass. Two reasons a
        // page would otherwise sit on an empty layer: an offscreen page that is mostly
        // a <video> (leanback) may not repaint on its own after a resize, and the
        // clock repaints only when its content changes — up to a second after its slot
        // came back. Castaway drops every stale-sized frame, so both read as a black
        // card until the page next chose to animate.
        win.webContents.invalidate();
      }
      return;
    }
    case 'touch':
      dispatchTouch(msg);
      return;
    case 'pointer':
      dispatchPointer(msg);
      return;
    case 'wheel': {
      const win = getWindow(msg.surface);
      if (!win) return;
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
    case 'probe': {
      // Test-only: let castaway ask a page a question. Kept deliberately thin — it
      // evaluates and reports, and owns no behaviour of its own.
      const win = getWindow(msg.surface);
      if (!win) {
        send({ type: 'probe-result', id: msg.id, value: '"no window"' });
        return;
      }
      win.webContents
        .executeJavaScript(`JSON.stringify(${msg.expression})`, true)
        .then((value) => send({ type: 'probe-result', id: msg.id, value: value ?? 'null' }))
        .catch((e) => send({ type: 'probe-result', id: msg.id, value: JSON.stringify(String(e)) }));
      return;
    }
    case 'quit':
      shutdown();
      return;
    default:
      log('warn', `unknown command ${msg.type}`);
  }
}

let shuttingDown = false;
function shutdown() {
  // 'end' and 'close' both arrive on a dropped socket; quitting twice is harmless but
  // releasing the lent textures twice is not.
  if (shuttingDown) return;
  shuttingDown = true;
  // Return every lent frame before going: Chromium asserts on a texture destroyed while
  // still lent out, and an assert here is a crash rather than a clean exit.
  for (const [, lent] of inflight) {
    try {
      lent.texture.release();
    } catch {
      /* already gone */
    }
  }
  inflight.clear();
  log('info', `shutting down; ${drops} frames dropped this session`);
  app.quit();
}

let appReady = false;
const queued = [];
let buffered = '';
wire.setEncoding('utf8');
wire.on('data', (chunk) => {
  buffered += chunk;
  let newline;
  while ((newline = buffered.indexOf('\n')) >= 0) {
    const line = buffered.slice(0, newline).trim();
    buffered = buffered.slice(newline + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      log('warn', `unparseable command: ${e}`);
      continue;
    }
    // Before `app.whenReady()` there is no window to act on, and `BrowserWindow` throws
    // rather than waiting. Dropping the command there would lose the very first
    // `navigate` to a startup race — a black panel with nothing in the log to explain it.
    if (!appReady) {
      queued.push(msg);
      continue;
    }
    try {
      handle(msg);
    } catch (e) {
      log('warn', `command failed: ${e}`);
    }
  }
});
// castaway vanishing is a quit, not an error loop — this is the backstop that stops an
// orphaned browser holding the GPU and the profile lock. Both events, because a peer that
// dies rather than closing cleanly delivers only `close`.
wire.on('end', shutdown);
wire.on('close', shutdown);

app.whenReady().then(async () => {
  await readyComponents();
  installAdblock(session.defaultSession);
  appReady = true;
  send({ type: 'ready', pid: process.pid });
  for (const msg of queued.splice(0)) {
    try {
      handle(msg);
    } catch (e) {
      log('warn', `queued command failed: ${e}`);
    }
  }
});

app.on('window-all-closed', () => {
  /* the panel decides when we exit, not the window count */
});
