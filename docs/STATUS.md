# Build Status — autonomous session 2026-07-23

Snapshot for our next sync. Companion to DECISION-LOG.md (why) and OPEN-QUESTIONS.md
(what needs you). Everything below builds with `cargo build`, passes `cargo test`
(~130 tests), passes `cargo clippy --all-targets -- -D warnings`, and `nix build`
produces a running binary.

## What exists (15 crates, workspace per architecture-substrate.md §2)

| Crate | State |
|---|---|
| `core` | Done. Traits (`SourceAdapter`, `Pipeline`, `DisplayControl`, `MiracastBackend`), `SessionEvent`/`FrameSource`, newtypes, last-writer-wins `SessionManager`. |
| `substrate-ssdp` | Done. Pure M-SEARCH/NOTIFY layer + UDP 1900 `Responder`. |
| `substrate-mdns` | Done. `mdns-sd` wrapper, validated `MdnsService`. |
| `substrate-rtsp` | Done. `rtsp-types` framing + CSeq + `ByteTransform` slot. |
| `substrate-rtp` | Done. RFC 3550 parse + gap-skipping reorder buffer. |
| `crypto-cast-auth` | Done. RSA device-auth signer (SHA1/256, PKCS1v15). |
| `crypto-fairplay` | Boundary stub. fp-setup typestate; key derivation = `NotImplemented` (Q1). |
| `proto-dlna` | **Live.** AVTransport/RenderingControl/ConnectionManager SOAP; cast-a-video works. |
| `proto-cast` | Core done. Framing, JSON, device-auth, media LOAD, mirroring negotiation + AES-CTR. TLS actor pending (Q15). |
| `proto-spotify` | Onboarding live. Advertise + `getInfo` + `addUser` DH/blob decrypt. Playback deferred (Q9). |
| `proto-airplay` | Control plane done. Ads + `/info` + RTSP dispatch. Media gated on FairPlay/pairing (Q1). RTSP actor pending (Q15). |
| `proto-dial` | **Live launch** + pure Lounge bind-channel parser/mapping. Lounge HTTP client pending. |
| `pipeline` | **Render path real.** Null backend (default) + wgpu compositor + ffmpeg decoder + RenderPipeline + winit kiosk behind `render`/`ffmpeg`/`kiosk` features. cef still a stub (Q6). |
| `control-display` | Null backend + Dell RS-232 frame encoder (opcodes placeholder, Q14). |
| `input-touch` | `TouchSource` trait + null; evdev/winuser feature stubs. |
| `app` | **Runs.** One HTTP host (DLNA+Spotify+DIAL) + one SSDP + one mDNS + session mgr. TOML config. |

## Verified working end-to-end (curl against `cargo run`)
- DLNA description + SOAP (`GetTransportInfo` → `NO_MEDIA_PRESENT`).
- Spotify `getInfo` (returns DH public key), advertised on mDNS.
- DIAL `dd.xml` with correct `Application-URL` header; launch flips state to running.

## Render path — actual pixel output (GPU-verified)
Behind `--features render` (+ `ffmpeg`/`kiosk`); needs the native devShell (`nix develop`).
- **wgpu compositor** renders textured-quad layers with transforms/z/opacity/blend on the
  RX 7900 XTX (RADV). Proven by offscreen readback tests (full-screen fill; corner PiP).
- **ffmpeg decoder** decodes a real clip to RGBA frames (verified vs an ffmpeg testsrc).
- **Full pipeline**: `Play(url)` → decode → GPU composite → **colored pixels read back**
  (`play_url_decodes_and_composites_pixels`). This is the "actual output rendering" answer.
- **Kiosk**: winit borderless-fullscreen surface path — compile-verified (not run on the
  dev box's live display). Encoded-mirror decode (AirPlay/Cast frames) is the next step.
- Run it: `nix develop --command cargo run -p castaway --features render` (opens a
  fullscreen window; cast a video via DLNA to see it decode+display).

## Biggest open items (see OPEN-QUESTIONS.md)
1. **Q15** — Cast TLS actor + AirPlay RTSP actor (the "make it connect" work; pure cores ready).
2. **Q1** — FairPlay-SAP + AirPlay pairing captures (gates AirPlay mirroring).
3. **Q16** — real pipeline (ffmpeg → wgpu → kiosk) behind the feature flags.
4. **Q2/Q11** — real Cast device cert for Chrome to accept auth.
5. **Q12/Q13** — Cast mirroring RTP receive loop + IV validation.

## CEF browser + adblock + YouTube (behind the `cef` feature)
The doc's "boss fight" is won — CEF builds, links, and **runs** reproducibly against nixpkgs
`cef-binary` (flake `cefDist` + `CEF_PATH`; no download/patchelf). Proven with screenshots:
- `pipeline::cef_browser` — offscreen Chromium via cef-rs; renders real pages headlessly
  (SwiftShader) → `on_paint` BGRA → `CefFrameSink`. Subprocess entry point, TV user-agent.
- `pipeline::cef_adblock` — Brave adblock-rust in CEF's `ResourceRequestHandler`, cancels +
  **logs** blocked requests (`castaway::adblock`). `easylist::load_or_fetch` fetches+caches
  EasyList, falls back to cache then compact built-in.
- **YouTube**: `youtube.com/tv` renders the leanback cast-receiver UI (TV UA), with EasyList
  blocking YouTube's ad requests (doubleclick instream `ad_status.js`, googleads id/tracking).
- `RenderLoop::upload_browser` feeds CEF frames into the compositor `Browser` layer.

**Task 16 (the app-main merge) is DONE and smoke-verified** (D24). Build `castaway` with
`--features cef`: `main` bootstraps CEF first (subprocess re-exec), the winit kiosk loop pumps
CEF each redraw via `pipeline::BrowserHost` and uploads new paints to the `Browser` layer, and
DIAL launch/stop (`DialEvent`) navigates/hides the browser over a `BrowserCommand` channel —
launch body → `youtube.com/tv?<pairing params>` so the phone binds to this screen. Verified
end-to-end on headless Xvfb with real network: DIAL POST → leanback UI composited on the kiosk
surface (screenshot), ad request blocked, DIAL DELETE → attract scene back, relaunch works,
ctrl-c → clean CEF/service shutdown. Also fixed en route: 4K-surface wgpu limits (the panel is
3840×2160 — would have crashed on first boot) and ctrl-c being swallowed by Chromium's SIGINT
handler. Still needs the physical box: real display/GPU present path, audio, and touch.

## Design decisions worth your review
D7 (router composition vs SourceAdapter), D9 (hand-written prost, no protoc), D10 (Spotify
scope), D16 (socket protocols advertise-gated). All in DECISION-LOG.md.
