// Measures what this Electron build can actually decode, and prints it as JSON.
//
// This exists because D36 rests on a claim about somebody else's build flags: that
// official Electron ships `proprietary_codecs=true ffmpeg_branding="Chrome"` where every
// prebuilt CEF does not (GAPS G55, which measured CEF failing this exact probe). A build
// flag is not evidence; `isTypeSupported` is. Run it against any candidate Electron —
// nixpkgs, castLabs ECS, a bumped version — before trusting it:
//
//   electron browser-host/codec-probe.js
//
// Exits non-zero if H.264 or AAC is missing, so it can gate a build rather than merely
// inform one.
'use strict';

const { app, BrowserWindow, dialog } = require('electron');

dialog.showErrorBox = (title, content) => {
  process.stderr.write(`codec-probe: ${title}: ${content}\n`);
};

// The pairs that matter, and why each is in the list:
//   avc1/mp4a — G55's gap: YouTube live and low-view content often has no other
//               rendition, and every commercial CAF receiver (G56) streams them.
//   vp9/av01/opus — what CEF *could* already play, kept so a regression shows up as a
//               difference rather than a uniform failure.
const CODECS = [
  ['video/mp4; codecs="avc1.42E01E"', 'H.264 baseline'],
  ['video/mp4; codecs="avc1.640028"', 'H.264 high'],
  ['audio/mp4; codecs="mp4a.40.2"', 'AAC-LC'],
  ['video/mp4; codecs="vp09.00.10.08"', 'VP9'],
  ['video/mp4; codecs="av01.0.04M.08"', 'AV1'],
  ['audio/webm; codecs="opus"', 'Opus'],
  ['audio/mpeg', 'MP3'],
];
const REQUIRED = ['H.264 baseline', 'H.264 high', 'AAC-LC'];

app.whenReady().then(async () => {
  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  await win.loadURL('data:text/html,<title>probe</title>');
  const results = await win.webContents.executeJavaScript(`
    (${JSON.stringify(CODECS)}).map(([mime, label]) => ({
      label,
      mime,
      // Both questions, because they disagree in exactly the interesting case: a build
      // can list a codec for <video> and refuse it for MSE, and adaptive streaming is
      // all MSE.
      mediaSource: (typeof MediaSource !== 'undefined')
        ? MediaSource.isTypeSupported(mime) : null,
      canPlayType: document.createElement('video').canPlayType(mime),
    }))
  `);

  const verdict = { version: process.versions.electron, chrome: process.versions.chrome, results };
  process.stdout.write(JSON.stringify(verdict, null, 2) + '\n');

  const missing = results.filter(
    (r) => REQUIRED.includes(r.label) && !r.mediaSource && !r.canPlayType
  );
  for (const r of missing) {
    process.stderr.write(`codec-probe: MISSING ${r.label} (${r.mime})\n`);
  }
  app.exit(missing.length ? 1 : 0);
});
