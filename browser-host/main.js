// The Electron side of the D36 spike (Q40): an offscreen shared-texture browser whose
// frames are consumed by a Rust process over a line protocol.
//
// This file is deliberately dependency-free — Electron APIs only, no npm tree (D36's
// condition on letting JS into the repo at all). It is not yet the production host; it
// exists to prove the one thing the port is gated on: that `useSharedTexture` paint
// events can cross to the compositor as GPU handles and come out as correct pixels.
//
// Protocol (JSON lines):
//   out (stdout): {type:"ready", pid}
//                 {type:"paint", id, pixelFormat, width, height, modifier, planes:[{fd,stride,offset,size}]}
//                 {type:"drop", id}            — backpressure released this frame unsent
//                 {type:"no-texture"}          — paint arrived without a texture: the
//                                                platform did not do shared-texture OSR,
//                                                which is a spike FAILURE, said out loud
//   in  (stdin):  {type:"release", id}         — consumer is done sampling; recycle
//                 {type:"quit"}
//
// Frame lifetime: a texture is valid until release(). Chromium recycles the buffer after
// that, so releasing before the consumer has finished sampling is a tear. The consumer
// acks; we release. If the consumer falls behind, frames are dropped *here*, newest kept
// pending, per ground rule 4's drop-late-frames.

'use strict';

const { app, BrowserWindow, dialog } = require('electron');

// Electron's default uncaught-exception handler is `dialog.showErrorBox`, which on a
// developer's desktop is a modal popup and on an unattended panel is a dialog nobody
// will ever dismiss. Neither is acceptable: failures belong on stderr, where the
// supervising process can read them and act.
dialog.showErrorBox = (title, content) => {
  process.stderr.write(`browser-host: ${title}: ${content}\n`);
};
process.on('uncaughtException', (err) => {
  process.stderr.write(`browser-host: uncaught: ${err && err.stack}\n`);
  app.exit(1);
});
process.on('unhandledRejection', (err) => {
  process.stderr.write(`browser-host: unhandled rejection: ${err}\n`);
  app.exit(1);
});

const WIDTH = parseInt(process.env.SPIKE_WIDTH || '3840', 10);
const HEIGHT = parseInt(process.env.SPIKE_HEIGHT || '2160', 10);
const FRAME_RATE = parseInt(process.env.SPIKE_FPS || '60', 10);
const MAX_PENDING = 3;

/** Frames handed to the consumer and not yet released, id → texture. */
const pending = new Map();
let seq = 0;
let drops = 0;

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

// The test page: a full-viewport canvas repainted every rAF, so paints keep flowing and
// every frame's center pixel is a pure function of its frame index — which is what lets
// the Rust side assert "correct pixels", not just "some pixels".
const PAGE = `<!doctype html>
<meta charset="utf-8">
<style>html,body{margin:0;height:100%;overflow:hidden}canvas{display:block}</style>
<canvas id="c"></canvas>
<script>
  const c = document.getElementById('c');
  c.width = innerWidth; c.height = innerHeight;
  const ctx = c.getContext('2d');
  let n = 0;
  function frame() {
    n += 1;
    // Center color: r walks, g fixed, b fixed. The consumer recomputes this from its
    // own counter and compares.
    ctx.fillStyle = 'rgb(' + (n * 8 % 256) + ',64,192)';
    ctx.fillRect(0, 0, c.width, c.height);
    ctx.fillStyle = '#fff';
    ctx.font = '160px monospace';
    ctx.fillText(String(n), 100, 300);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
</script>`;

app.whenReady().then(() => {
  const win = new BrowserWindow({
    width: WIDTH,
    height: HEIGHT,
    show: false,
    webPreferences: {
      offscreen: { useSharedTexture: true },
      // The page is ours and static, but the production host will render other
      // people's pages, so the spike runs the same posture it would.
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  // The constructor's width/height are *window* dimensions and get clamped to the
  // display's work area even for an offscreen window — which silently renders the panel
  // resolution smaller than asked and would have made a 4K pacing claim false. Setting
  // the content size afterwards is not subject to that clamp.
  win.setContentSize(WIDTH, HEIGHT);
  win.webContents.setFrameRate(FRAME_RATE);

  win.webContents.on('paint', (event) => {
    if (!event.texture) {
      // Software OSR fallback = the platform did not give us a GPU texture. That is
      // exactly the worst case D36 records; make it unmissable.
      send({ type: 'no-texture' });
      return;
    }
    if (pending.size >= MAX_PENDING) {
      // Consumer is behind: latency beats freshness losing to backlog — drop this
      // frame now rather than queueing staleness (ground rule 4).
      drops += 1;
      send({ type: 'drop', id: seq + 1 });
      seq += 1;
      event.texture.release();
      return;
    }
    seq += 1;
    const id = seq;
    const info = event.texture.textureInfo;
    if (id === 1) {
      // One structural dump so a field rename in a future Electron is a diff in the
      // log, not a mystery.
      process.stderr.write(
        'browser-host: textureInfo shape: ' +
          JSON.stringify(info, (k, v) =>
            Buffer.isBuffer(v) ? `<Buffer ${v.length}>` : v
          ) +
          '\n'
      );
    }
    // Linux delivers a NativePixmapHandle: per-plane fds plus a DRM format modifier,
    // nested under `handle.nativePixmap`. Windows/macOS put a different member here
    // (NT handle / IOSurface), so read defensively rather than assuming the platform.
    const pixmap = info.handle && info.handle.nativePixmap;
    if (!pixmap) {
      send({
        type: 'no-texture',
        detail: 'textureInfo.handle has no nativePixmap: ' + JSON.stringify(info.handle),
      });
      pending.delete(id);
      event.texture.release();
      return;
    }
    pending.set(id, event.texture);
    send({
      type: 'paint',
      id,
      pixelFormat: info.pixelFormat,
      width: info.codedSize.width,
      height: info.codedSize.height,
      // u64 as a string: a DRM format modifier does not fit in a JS number.
      modifier: String(pixmap.modifier),
      planes: pixmap.planes.map((p) => ({
        fd: p.fd,
        stride: p.stride,
        offset: p.offset,
        size: p.size,
      })),
    });
  });

  win.loadURL('data:text/html;base64,' + Buffer.from(PAGE).toString('base64'));
  send({ type: 'ready', pid: process.pid });
});

let buffered = '';
process.stdin.on('data', (chunk) => {
  buffered += chunk;
  let newline;
  while ((newline = buffered.indexOf('\n')) >= 0) {
    const line = buffered.slice(0, newline);
    buffered = buffered.slice(newline + 1);
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      process.stderr.write('browser-host: unparseable line from consumer\n');
      continue;
    }
    if (msg.type === 'release') {
      const texture = pending.get(msg.id);
      if (texture) {
        pending.delete(msg.id);
        texture.release();
      }
    } else if (msg.type === 'quit') {
      process.stderr.write(`browser-host: quitting, drops=${drops}\n`);
      app.quit();
    }
  }
});
// The consumer vanishing is a quit, not an error loop.
process.stdin.on('end', () => app.quit());
