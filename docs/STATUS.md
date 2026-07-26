# Build Status — autonomous session 2026-07-23

Snapshot for our next sync. Companion to DECISION-LOG.md (why) and OPEN-QUESTIONS.md
(what needs you). Everything below builds with `cargo build`, passes `cargo test`
(~150 tests), passes `cargo clippy --all-targets -- -D warnings`, `nix build` produces a
running binary, and `nix build .#checks.x86_64-linux.integration-vm` passes the two-VM
integration test.

## What exists (16 crates, workspace per architecture-substrate.md §2)

| Crate | State |
|---|---|
| `core` | Done. Traits (`SourceAdapter`, `Pipeline`, `DisplayControl`, `MiracastBackend`), `SessionEvent`/`FrameSource`, newtypes, last-writer-wins `SessionManager`. |
| `substrate-ssdp` | Done. Pure M-SEARCH/NOTIFY layer + UDP 1900 `Responder`. |
| `substrate-mdns` | Done. `mdns-sd` wrapper, validated `MdnsService`. |
| `substrate-rtsp` | Done. `rtsp-types` framing + CSeq + `ByteTransform` slot + AirPlay's bare-path request-URIs. |
| `substrate-rtp` | Done. RFC 3550 parse + gap-skipping reorder buffer. |
| `crypto-cast-auth` | Done. RSA device-auth signer (SHA1/256, PKCS1v15). |
| `crypto-fairplay` | Boundary stub. fp-setup typestate; key derivation = `NotImplemented` (Q1). |
| `proto-dlna` | **Live.** AVTransport/RenderingControl/ConnectionManager SOAP; cast-a-video works. |
| `proto-cast` | **Live, both paths.** Framing, JSON, device-auth, media LOAD, and a TLS actor on 8009 driven end-to-end in the VM test. Mirroring is complete: OFFER/ANSWER negotiation, RTP reassembly, RTCP feedback, AES-CTR decrypt, and a UDP actor — differential-tested against openscreen's own packetizer (Q12/Q13). Dev device key (Q2/Q11). |
| `proto-spotify` | Onboarding live. Advertise + `getInfo` + `addUser` DH/blob decrypt. Playback deferred (Q9). |
| `proto-airplay` | **Control plane live.** Ads + `/info` + RTSP dispatch, served over real sockets on 7000/7011. Media plane still gated on FairPlay/pairing (Q1) — pairing answers `501`. |
| `sponsorblock` | **Live.** Hash-prefix lookup, category/overlap filtering, and the when-to-skip planner — pure, fixture-tested. Driven by an actor in `app` that binds to our own screen as a Lounge remote. |
| `proto-dial` | **Live launch, and a phone really plays through it** (`yt-selfplay`), including the attach-to-a-running-app path via a published `<screenId>`. Gated on a launch target: a build with no browser does not advertise DIAL. Pure Lounge bind-channel parser/mapping kept for a non-CEF fallback; no native Lounge client. |
| `pipeline` | **Render path real.** Null backend (default) + wgpu compositor + ffmpeg decoder + RenderPipeline + winit kiosk behind `render`/`ffmpeg`/`kiosk` features. cef still a stub (Q6). |
| `control-display` | Null backend + Dell RS-232 frame encoder (opcodes placeholder, Q14). |
| `input-touch` | `TouchSource` trait + null; evdev/winuser feature stubs. |
| `app` | **Runs.** One HTTP host (DLNA+Spotify+DIAL) + one SSDP + one mDNS + session mgr. TOML config. |

## Verified working end-to-end (tier-2 VM test, no human in the loop)
`nix build .#checks.x86_64-linux.integration-vm` boots the receiver from the real NixOS
module in one VM and drives it from a *second* VM over a real LAN with scripted senders —
so the deploy path and the tested path are the same path, and discovery is real rather
than hidden by loopback. Each of these asserts the sender's view **and** greps the
receiver's journal, so the event is proven to cross adapter → session manager → pipeline:

- **SSDP**: M-SEARCH answered; every advertised `LOCATION` is fetched from the other host.
- **DIAL**: that a *browser-less* build advertises nothing, mounts nothing, and says why.
  Launch/stop semantics are proto-dial's own tests; the real path is `yt-selfplay` below.
- **DLNA**: `SetAVTransportURI`/`Play`/`Pause`/`Stop` walk
  `NO_MEDIA_PRESENT → PLAYING → PAUSED_PLAYBACK → STOPPED`.
- **Cast**: a hand-rolled CASTv2 sender does TLS → CONNECT → PING → GET_STATUS → LAUNCH
  `CC1AD845` → LOAD → PAUSE → CLOSE; the LOAD reaches the pipeline and the CLOSE ends the
  session.
- **AirPlay**: pipelined `OPTIONS` + `GET /info` in one write (bare-path URIs), the `/info`
  binary plist parses, pairing answers `501` rather than faking success, `TEARDOWN` ends
  the session — on both 7000 and 7011.
- **mDNS**: `_spotify-connect._tcp`, `_googlecast._tcp`, `_airplay._tcp`, and `_raop._tcp`
  are all browsable from the sender with the ports that actually answered.

## Which package to run
- `packages.default` — portable, no renderer, no browser. Serves and discovers; **cannot**
  play YouTube, and honestly declines to advertise DIAL (D27). What CI builds.
- `packages.castaway-cef` — the Linux kiosk: render pipeline + CEF browser, wrapped with
  `CEF_PATH`/`LD_LIBRARY_PATH` so it runs outside the devShell. **This is the one to deploy
  on Linux** (`services.castaway.package`). Verified 2026-07-26: built from the flake, run
  headless on Xvfb, and passed both `yt-selfplay` modes with real video composited at 4K.
- `packages.castaway-windows-cef` — the Windows deploy artifact, cross-compiled.

## A YouTube cast, with no phone (`nix run .#yt-selfplay -- http://<receiver>:8080`)
The one path a VM test cannot cover: YouTube's Lounge servers are a third party to the
session, so this needs the real internet and a running `--features cef` receiver. It is a
scripted phone — DIAL launch with a `pairingCode` it invented, wait for the receiver's
page to register that code with YouTube, bind to the Lounge session as a remote control,
queue videos, and assert the screen actually plays them. **Verified 2026-07-26** against
the CEF kiosk on Xvfb: three taps, each confirmed playing, plus 4K screenshots of real
decoded video on the composited surface.

`--reconnect` covers the cast that is *not* the first one: a sender that arrives after the
app is already running never launches it, so it can only find the screen from the
`<screenId>` in the app-info XML. **That is the bug this hunt actually found** — we
published nothing, so every cast after the first connected and could never be queued to,
which is exactly "it doesn't play videos as I browse". Nothing sends DELETE in practice,
so `running` is where a receiver stays.

Why it asserts what it asserts, all learned the hard way against the live service:
- **The clock, not the state code.** `onStateChange` says PLAYING without saying *which*
  video, so a screen still happily rolling the previous tap satisfies it — which is
  exactly the "I browsed and it kept playing the first thing" failure. It asks
  `getNowPlaying` and requires the position to *advance* on the video it queued. (The
  documented state set is also incomplete: `1081` shows up with playback plainly running.)
- **Every tap, not just the first.** Queueing a second video without `videoId` set is
  read as an edit of the existing playlist, and the screen keeps playing what it had.
  Casting is not one launch; it is a session someone browses.

Its failure message is the point: a receiver that launched nothing fails at "the screen
never registered our pairing code", which is the exact silent failure DIAL alone cannot
distinguish from success.

`--expect-skip` proves SponsorBlock end to end, asserting a *discontinuity* — playback
advancing further than wall time did, which only a seek can do.

## SponsorBlock (`[sponsorblock]` in castaway.toml, needs the `cef` build)
**Live, verified 2026-07-26** — segments loaded, segment skipped, toast on screen over
real video. The receiver attaches to its own page as a second Lounge remote and sends
`seekTo`; there is no hook into the player and no injected JavaScript (D29).

```toml
[sponsorblock]
enabled = true
categories = ["sponsor", "selfpromo", "music_offtopic"]  # the default set
minimum_seconds = 1.0
toast = true
```

Also valid: `interaction`, `intro`, `outro`, `preview`, `filler`, `exclusive_access`. A
name that is not one of these is warned about at startup — categories parse leniently so
the API can add one without breaking a response, which would otherwise make a config typo
a silent no-op.

Lookups use the hash-prefix endpoint: the server sees four hex characters of
`sha256(videoId)` and never the video. The database is CC BY-NC-SA — non-commercial use
fits, attribution rides on the toast, and segments are deliberately never written to disk
(that would be redistribution). YouTube's own ads are *not* skipped; nothing mutes.

## Render path — actual pixel output (GPU-verified)
Behind `--features render` (+ `ffmpeg`/`kiosk`); needs the native devShell (`nix develop`).
- **wgpu compositor** renders textured-quad layers with transforms/z/opacity/blend on the
  RX 7900 XTX (RADV). Proven by offscreen readback tests (full-screen fill; corner PiP).
- **ffmpeg decoder** decodes a real clip to RGBA frames (verified vs an ffmpeg testsrc),
  from a URL (`decode`) *or* from pushed frames with no container at all (`decode_stream`).
- **Full pipeline**: `Play(url)` → decode → GPU composite → **colored pixels read back**
  (`play_url_decodes_and_composites_pixels`). This is the "actual output rendering" answer.
- **Encoded mirror**: `FrameSource::Encoded` → streaming decode → GPU composite → colored
  pixels (`encoded_mirror_decodes_and_composites_pixels`). Decode waits for a key frame
  (mirror sessions are joined mid-stream), carries the adapter's timestamps through the
  decoder's reorder buffer, and rebuilds swscale when the sender changes resolution.
- **Hardware decode** (`hwaccel` feature, Q20): VA-API → DMA-BUF → Vulkan import → NV12
  sampled in the shader, with **no copy anywhere**. `tests/hwaccel_zero_copy.rs` decodes a
  known colour on the dev box's RX 7900 XTX and asserts on the composited pixels, which is
  what catches a wrong tiling, wrong plane pitches, or the wrong colour matrix — all of
  which render a picture rather than an error. The hw/sw choice is runtime, not a build
  flag, and falls back to software mid-session with a log line. The Windows half
  (D3D11VA → shared NV12 texture → D3D12) is cross-compiled and DLL-closure-checked but
  needs the Dell to run.
- **Kiosk**: winit borderless-fullscreen surface path — compile-verified (not run on the
  dev box's live display).
- Run it: `nix develop --command cargo run -p castaway --features render` (opens a
  fullscreen window; cast a video via DLNA to see it decode+display).

## Biggest open items (see OPEN-QUESTIONS.md)
1. ~~**Q15** — Cast TLS actor + AirPlay RTSP actor.~~ **Done**: both listen, both are
   driven end-to-end by the VM test. What's left behind them is the media plane, below.
2. **Q1** — FairPlay-SAP + AirPlay pairing captures (gates AirPlay mirroring).
3. ~~**Q16** — real pipeline (ffmpeg → wgpu → kiosk) behind the feature flags.~~ **Mostly
   done**: all three `FrameSource` variants reach composited pixels in readback tests.
   What's left is the kiosk surface on the real panel.
4. ~~**Q20** — hardware-accelerated decode.~~ **Done on Linux**, proven zero-copy by an
   offscreen readback test; the Windows D3D11VA bridge is written and cross-compiled but
   unverified until the Dell.
5. **Q2/Q11** — real Cast device cert for Chrome to accept auth.
6. ~~**Q12/Q13** — Cast mirroring RTP receive loop + IV validation.~~ **Done**: the
   receive path is differential-tested against openscreen's `RtpPacketizer` +
   `FrameCrypto`, compiled from a pinned checkout by the `openscreen-rtp-fixtures` check.

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
