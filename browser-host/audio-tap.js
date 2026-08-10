// Routes the page's audio out to castaway instead of to the sound card.
//
// This is injected into the **main world** at document-start, the same way uBO scriptlets
// are, because it has to patch `HTMLMediaElement` before the page constructs any media.
//
// ## Why this exists
//
// CEF had `CefAudioHandler`, which handed the host PCM directly. Electron has no
// equivalent: left alone, a page plays through the system device, which means castaway
// cannot mix it with Spotify or AirPlay audio, cannot apply volume, and — the one that
// actually shows — has no timeline for it, so nothing can measure lip-sync against the
// frames it is compositing.
//
// WebAudio gets it back. A `MediaElementAudioSourceNode` *removes* the element from the
// normal output path: once an element is routed through one, its audio only goes where
// the graph sends it. So the graph sends it here and nowhere else, and the browser
// process itself is silent by construction rather than by muting.
//
// ## The timestamp is the point
//
// Every block carries the media element's `currentTime` at capture. That is what makes
// A/V sync measurable: the frames castaway composites carry Chromium's own paint
// timestamp, and pairing the two turns drift into a number. They are NOT one clock —
// the paint timestamp is the compositor's, on an origin Chromium chooses — so castaway
// measures the offset at the first pairing and subtracts it (#278). Without this the
// browser's picture and its sound are two streams with no relationship anyone can
// inspect.
'use strict';

(() => {
  if (window.__castawayAudioTap) return;
  window.__castawayAudioTap = true;

  const SAMPLE_RATE = 48000;
  // 1024 frames at 48 kHz is ~21 ms: small enough that a block is not itself a
  // perceptible delay, large enough that the binding is not called thousands of times a
  // second.
  const BLOCK = 1024;

  const ctx = new AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: 'playback' });
  const tapped = new WeakSet();

  // A worklet rather than the deprecated ScriptProcessorNode: ScriptProcessor runs on the
  // main thread, so a busy page (which leanback is) drops audio blocks under exactly the
  // load where sync matters most.
  const workletSource = `
    class CastawayTap extends AudioWorkletProcessor {
      constructor() {
        super();
        this.buf = [];
        this.filled = 0;
      }
      process(inputs) {
        const input = inputs[0];
        if (!input || input.length === 0) return true;
        const channels = input.length;
        const frames = input[0].length;
        // Interleave: the consumer wants one stream, not N planes.
        const out = new Float32Array(frames * channels);
        for (let f = 0; f < frames; f++) {
          for (let c = 0; c < channels; c++) out[f * channels + c] = input[c][f];
        }
        this.port.postMessage({ pcm: out, channels, frames }, [out.buffer]);
        return true;
      }
    }
    registerProcessor('castaway-tap', CastawayTap);
  `;
  const workletUrl = URL.createObjectURL(new Blob([workletSource], { type: 'text/javascript' }));

  let ready = ctx.audioWorklet.addModule(workletUrl).catch((e) => {
    // Reported rather than swallowed: silent browser audio is indistinguishable from a
    // page that simply is not playing anything.
    if (window.__castawayAudioError) window.__castawayAudioError('worklet: ' + e);
    throw e;
  });

  function attach(el) {
    if (tapped.has(el)) return;
    tapped.add(el);
    ready = ready.then(() => {
      let source;
      try {
        source = ctx.createMediaElementSource(el);
      } catch (e) {
        // Already routed through another graph, or cross-origin without CORS. Leave the
        // element alone: taking its audio and failing is worse than not taking it.
        if (window.__castawayAudioError) window.__castawayAudioError('source: ' + e);
        return;
      }
      const node = new AudioWorkletNode(ctx, 'castaway-tap', {
        numberOfInputs: 1,
        numberOfOutputs: 0,
      });
      node.port.onmessage = (ev) => {
        const { pcm, channels } = ev.data;
        if (!window.__castawayAudio) return;
        // Float32 → bytes → base64. CDP bindings carry strings only, so this is the
        // price of the channel; at ~21 ms blocks it is a few KB per call.
        const bytes = new Uint8Array(pcm.buffer);
        let binary = '';
        for (let i = 0; i < bytes.length; i += 0x8000) {
          binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
        }
        window.__castawayAudio(
          JSON.stringify({
            pcm: btoa(binary),
            channels,
            sampleRate: ctx.sampleRate,
            // The media clock, not the wall clock: this is what a frame's presentation
            // time has to be compared against.
            mediaTime: el.currentTime,
            paused: el.paused,
          })
        );
      };
      // Deliberately NOT connected to ctx.destination: the point is that the browser
      // process makes no sound of its own. castaway mixes and plays it.
      source.connect(node);
    });
  }

  // Elements that already exist, and every one created afterwards.
  const scan = () => document.querySelectorAll('video,audio').forEach(attach);
  scan();
  new MutationObserver(scan).observe(document.documentElement || document, {
    childList: true,
    subtree: true,
  });
  // Leanback constructs its player element lazily and sometimes off-DOM, so catch it at
  // the point it starts playing too.
  document.addEventListener('play', (e) => attach(e.target), true);

  // Autoplay policy: a kiosk has no user gesture to offer, so the context has to be
  // resumed explicitly or the graph never pulls.
  const resume = () => ctx.state === 'suspended' && ctx.resume();
  resume();
  document.addEventListener('play', resume, true);
})();
