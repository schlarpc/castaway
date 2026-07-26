# Open Questions

Things I could not resolve autonomously and want to settle at the next sync.
Grouped by subsystem. Each: the question, why it's blocked, and my current default.

## Fixtures / reverse engineering (needs `~/re-shell`)

- **Q1 — FairPlay-SAP byte captures.** The AirPlay `/fp-setup` v3 handshake (~568-byte
  flow) needs real captures from `airplay2-receiver`/UxPlay against a live iOS sender.
  I've modeled the message *shape* and stubbed the crypto module boundary. Default:
  `crypto-fairplay` exposes the handshake state machine + typed messages but returns a
  `NotImplemented` error at the actual key-derivation step until we land a fixture.
- **Q2 — Cast device-auth cert material.** `crypto-cast-auth` needs a real gen-1 device
  cert + key to sign the CASTv2 `AuthChallenge`. At n=1 this is "a fixed local input"
  (per hackerspace notes). Default: signer trait takes cert/key as configured bytes;
  ships with a self-signed dev cert for tests, real material provisioned out of band.
- **Q3 — YouTube Lounge bind-channel transcript.** Need a real `yt-cast-receiver`
  session capture (the BrowserChannel framing: `RID`/`AID`/`SID`/`gsessionid`, chunked
  length-prefixed JSON). I've implemented the documented framing + command parser;
  want a golden transcript to validate chunk boundaries + the noop/heartbeat cadence.

## Design questions for you

- **Q4 — Config format + source.** I'm defaulting to a TOML config file
  (`castaway.toml`) deserialized with serde, with a typed `Config` in `app`. Confirm
  you want TOML (vs. env-only, or a Nix-generated config). Friendly name, which
  protocols to enable, display-control backend selection all live here.
- **Q5 — mDNS 5353 ownership.** Docs say own 5353 ourselves (disable Avahi on the
  kiosk). On the NixOS dev box Avahi may be running. Default: `substrate-mdns` uses
  `mdns-sd` which joins with SO_REUSEADDR; if it fights Avahi locally I'll note it.
  Do you want a NixOS module fragment that disables Avahi on the deploy box?
- **Q6 — CEF binding pull-in.** `cef-rs` (`tauri-apps/cef-rs`, `cef-v150.x`) is heavy
  and cross-links as a "boss fight." Default: `cef` feature is OFF; Lounge/PiP use a
  stubbed browser (headless player). I have NOT added the `cef` dependency yet to keep
  the tree buildable. Confirm when you want the real CEF slice wired + the Windows-CI
  escape hatch turned on.

## Spotify

- **Q9 — Spotify playback backend.** Onboarding is done (advertise → `getInfo` →
  `addUser` blob decrypt, librespot-compatible). Post-pairing playback needs the
  "dealer" WebSocket (control) + audio pull from the CDN with the AP-login step that
  turns the decrypted blob into stored credentials — a large stack that needs a Premium
  account. Deferred. Current behavior: pairing succeeds, credentials are decrypted and
  logged, no `SessionEvent` emitted. Confirm you want to invest here vs. leave at
  "appears in the picker + pairs."
- **Q10 — Spotify blob wire-validation.** The DH + blob crypto is tested by round-trip
  (our encrypt vs. decrypt), not against a real Spotify sender. Capture one `addUser`
  from a phone in `~/re-shell` to confirm the exact byte framing (iv/ciphertext/hmac
  split) matches before trusting it live.

## Cast

- **Q11 — Device-auth is required even for media-URL LOAD.** The pure Cast session
  answers `AuthChallenge` via a `DeviceAuthResponder` trait; without a signer it returns
  `AuthError`, which real senders may reject before LOAD. So even the "simple" media path
  needs `crypto-cast-auth` (task 5 / Q2) wired with real cert material to work against
  Chrome. Local testing can use a dev cert; Chrome may still refuse an untrusted chain.

## Deferred (per docs, not blockers)

- **Q7 — Miracast backend.** `proto-miracast` deferred (rule: get everything else
  working first; Wi-Fi P2P is the yak). Trait `MiracastBackend` lives in `core`; no
  backend impl yet. `backend-windows` is the intended first impl (cross-build).
- **Q8 — Zero-copy decode path.** MVP is decode→CPU AVFrame→wgpu upload. DXGI shared
  handle / dmabuf import is explicitly post-MVP. Not touching until the CPU path runs.

## Cast mirroring media plane

- **Q12 — Cast RTP receive + frame reassembly. RESOLVED.** `proto-cast::rtp` parses
  Cast's RTP framing (truncated frame/packet ids, reference frames, the adaptive-latency
  extension) and reassembles frames; `receiver` holds the sliding window, the checkpoint
  and the skip-ahead policy; `rtcp` builds the compound feedback (ACK bit vector, NACK
  loss fields, PLI, receiver reference time); `rtp_actor` is the UDP shell that composes
  them with a socket and a clock. No capture was needed in the end — see Q13.
- **Q13 — Per-frame IV derivation. RESOLVED.** The frame id's low 32 bits go at offset
  **8**, not 12: the last four bytes are the AES-CTR block counter, and putting the id
  there would have made it march through the keystream mid-frame. Verified rather than
  reasoned: `nix/openscreen-fixtures.nix` compiles openscreen's own `RtpPacketizer` and
  `FrameCrypto` from a pinned checkout, and `tests/openscreen_stream.rs` reassembles and
  decrypts the bytes they produce. A wrong nonce offset cannot pass that test.

  This is the pattern to reach for when a protocol detail is unverifiable by inspection:
  pin the reference implementation as a Nix derivation, compile the handful of
  translation units that produce the bytes, and check the output in as a fixture. It is
  much cheaper than a live capture and it cannot drift, because the Nix check
  regenerates it. Ground rule 9 forbids reference impls in the *shipping binary*; it
  does not forbid them as test oracles.

## App / hardware wiring

- **Q14 — Dell C6522QT RS-232 opcodes.** `control-display::dell` models the command frame
  (header/id/category/opcode/len/data/XOR-checksum) but the opcode bytes (power, input
  select) are placeholders. Confirm against Dell's C6522QT *RS232 External Control
  Application* manual before trusting on hardware.
- **Q15 — Cast TLS actor + AirPlay RTSP actor. RESOLVED.** Both socket actors are written
  and driven end-to-end by the tier-2 VM test: CASTv2 over TLS on 8009 with a self-signed
  cert the device-auth signs over, and AirPlay RTSP on 7000/7011. The post-pairing ChaCha20
  transform is still `Identity` — the `ByteTransform` seam is there, but there is nothing
  to key it with until Q1 lands. Both protocols stay OFF by default, now for a narrower
  reason than D16 gave: the *listeners* answer, but Cast's device key is a dev key
  (Q2/Q11) and AirPlay can't pair (Q1), so a sender that finds either still can't play.
- **Q16 — Real pipeline behind features.** Largely **RESOLVED**. `RenderPipeline` now
  covers all three `FrameSource` variants: `Url` demuxes and decodes, `Encoded` feeds a
  containerless decoder (`ffmpeg_decode::decode_stream`) from the adapter's frame channel,
  and `Decoded` forwards straight to the compositor. Each lands composited pixels in an
  offscreen readback test, so the render path is verified without a human or a panel.
  What is still open is not wiring but *hardware*: the winit kiosk surface has only ever
  been compile-checked and wants the C6522QT box. (Decode is no longer software-only —
  Q20 landed the zero-copy hardware path.)
- **Q20 — Hardware-accelerated decode. RESOLVED on Linux; Windows is compile-checked.**
  Behind the `hwaccel` feature (on by default for the Windows `cef` deploy artifact).

  The framing was right: this was never "turn on VAAPI", it was "who owns the decoded
  surface", and the answer changed types in `core`. `DecodedFrame` is now
  `FrameImage::{Cpu, Gpu}`; the GPU variant is an opaque `dyn GpuSurface` so `core` still
  cannot name a DMA-BUF or a DXGI handle, and a Windows `MiracastReceiver` can hand one
  over without `core` growing a `cfg`.

  What landed, against the slicing this question proposed:
  1. **Types + policy.** `FrameImage`, `GpuSurface`, `ColorInfo`, and `FallbackPolicy`.
  2. **Linux zero-copy, proven.** `av_hwframe_map` to `AV_PIX_FMT_DRM_PRIME` (no libva),
     imported via `VK_EXT_image_drm_format_modifier` + `VK_EXT_external_memory_dma_buf`.
     wgpu never requests those extensions, so the logical device is built in
     `hwaccel::vulkan_import` — wgpu-hal's own extension list plus the interop ones —
     and handed back through `Adapter::create_device_from_hal`.
  3. **NV12 sampling replaces swscale on that path**, via `TextureFormat::NV12` with
     `Plane0`/`Plane1` views and a matrix derived from the surface's own colorimetry.
  4. **Windows**: D3D11VA → `CopySubresourceRegion` into a shared NV12 texture →
     `CreateSharedHandle` → `ID3D12Device::OpenSharedHandle`. Written, cross-compiled,
     **not run** — see below.

  Things worth knowing before touching this again:
  - **`tests/hwaccel_zero_copy.rs` asserts on colour, not on the absence of errors.** It
    decodes a known solid colour and reads the composited pixels back off the dev box's
    RX 7900 XTX. That matters because nearly every way this path breaks produces a
    *picture*: a wrong DRM format modifier renders the right image with the wrong tiling,
    swapped plane pitches shift the chroma, the wrong matrix shifts the colour, and
    `VK_IMAGE_LAYOUT_UNDEFINED` on the import barrier is *permitted by the spec* to
    discard the contents entirely. Mesa preserves them for a DRM-modifier image; this test
    is what says so on this driver.
  - **The `AVFrame` reference in `DmaBufSurface` is load-bearing.** A DMA-BUF fd keeps the
    buffer object alive but does nothing to stop libavcodec handing the same VA surface to
    the next picture. That reference lives inside the Vulkan image's drop guard, so it is
    released exactly when wgpu retires the last submission that sampled it.
  - **`ffmpeg-sys-next` binds `hwcontext.h` and `hwcontext_drm.h` and stops.** There is no
    `AVD3D11VADeviceContext`; `hwaccel::d3d11va` declares it as a `#[repr(C)]` shim against
    libavutil's published ABI, reading only leading fields.
  - **Lossless H.264 (`-qp 0`) is undecodable by any fixed-function decoder.** It cost an
    hour of "why does VA-API refuse this"; test fixtures must use a normal CRF or the
    fixture itself forces the software fallback the test exists to rule out.

  Still open:
  - **The Windows bridge needs the Dell.** `nix build .#castaway-windows-hwaccel` keeps it
    compiling and its DLL closure checked, which is all Linux can do. Unverified in
    particular: whether the `D3D11_QUERY_EVENT` producer-side wait is sufficient
    synchronisation in practice (it should be — it blocks until the copy has retired
    before the handle is published — but a shared `ID3D11Fence` would be cheaper, and
    needs D3D11.4 interfaces `winapi` 0.3 does not declare), and whether the pool's
    `Arc`-count reuse gate is conservative enough under real frame latency.
  - **10-bit.** P010 is refused by `DmaBufSurface` rather than reinterpreted, so an HDR or
    10-bit sender falls back to software cleanly. Adding it is a second Vulkan format and
    a second shader variant, not a redesign.
  - **`Play(url)` restarts demuxing on a mid-stream fallback**, which for a file means
    seeking back to the start. Acceptable for a rare event; the mirror path has no such
    problem because it just resyncs on the next key frame.

## CEF / adblock / YouTube Lounge

- **Q17 — Filter-list source + refresh.** Default adblock is a compact built-in list; the real
  coverage comes from EasyList (proven: it blocks the video-ad loader). Decide how the kiosk gets
  a full list: bundle a snapshot (goes stale), fetch+cache from easylist.to on a timer (needs
  network), or a config path. Recommend fetch+cache with the compact list as offline fallback.
- **Q18 — YouTube Lounge via CEF (the actual plan).** With CEF working, the Lounge path is: on
  DIAL launch, navigate the offscreen browser to YouTube's TV surface (`https://www.youtube.com/tv`
  + the launch params/pairing code) and let the page do Lounge registration + playback itself
  (architecture §5 "double duty"). This replaces the native bind-channel client (the parser stays
  useful for a non-CEF fallback). Still to wire (task 16): feed CEF on_paint into the compositor
  Browser layer, and the DIAL-launch → navigate handoff. YouTube ad-blocking is an arms race —
  request blocking + JS help, but no guarantees.
  **ANSWERED 2026-07-26 — the plan works, and it is now tested.** `nix run .#yt-selfplay` drives
  the whole path with no phone and no human: the page registers the sender's pairing code with
  YouTube within ~3s of the launch, the session binds, and queued videos actually play (4K
  screenshots of decoded video on the composited surface). Two things fell out of building it:
  the leanback page takes its launch parameters as a plain **query string** on
  `youtube.com/tv?…` (no `#` fragment needed), and **in-stream ads still play** — EasyList
  blocks ad *requests*, not the ad segments the player itself serves, so a pre-roll ran before
  a queued video. That is the arms race this bullet predicted; skipping it is a separate
  mechanism from request blocking.
- ~~**Q32 — a Linux package with the browser in it.**~~ **DONE 2026-07-26.**
  `packages.castaway-cef` (`nix/linux-cef.nix`) is the Linux kiosk build: `--features cef`,
  built against the same flattened `cefDist` the devShell uses — hoisted to `cefDistFor` so
  the two cannot drift — and wrapped so `CEF_PATH` and `LD_LIBRARY_PATH` are set at *run*
  time. Both matter: `Cef::initialize` reads `CEF_PATH` at runtime to find the .pak/ICU/
  locales, and CEF re-execs the same binary for its subprocesses, so the wrapper has to be
  what runs. `services.castaway.package` documents it as the choice for a real display.
  `packages.default` stays the portable, browser-less build — the right thing for CI, and now
  honest about not offering YouTube (D27).
- ~~**Q33 — the leanback page's storage is per-launch.**~~ **WITHDRAWN 2026-07-26 — the premise
  was wrong.** The `generate_screen_id` calls that prompted this were from a *bare Chromium*
  probe run with a throwaway `--user-data-dir`, not from castaway. The kiosk's CEF profile is
  already persistent: `Cef::initialize` points `root_cache_path` at a stable
  `~/.cache/castaway/cef` (deliberately, for exactly this class of reason), and the profile on
  disk has a real `Default/Local Storage`. Measured directly: the screen id survives a full
  process restart — two separate runs published the same
  `f970ef4ce158…`. So the id we publish for senders to attach to (D28) is stable across
  reboots, and nothing needs changing. Left here as a correction rather than deleted, because
  "the receiver mints a new screen every launch" is a plausible-sounding claim that would have
  sent the next person down a hole.
- **Q34 — YouTube's own ads: `skipAd` is in, and the uBlock Origin approach does not port.**
  `skipAd` is implemented (D29): once an ad reports `isSkipEnabled` we press the screen's own
  skip button. Unskippable ads play and nothing is muted — a mute that failed to lift leaves
  a silent display, worse than the ad. Still unobserved live: no skippable ad has been served
  to a test session, though the command encoding is verified accepted.

  **The obvious next step — port uBO's scriptlets and subscribe to their lists — was
  investigated and does not do what it looks like it does.** uBO kills YouTube video ads with
  *scriptlets*, not network rules, because the ad media streams from the same `googlevideo`
  hosts as the content and the manifest is a field inside JSON the page already fetched.
  Their rules (fetched 2026-07-26) rewrite the **`/player`** response:

      www.youtube.com##+js(trusted-replace-fetch-response, '"adPlacements"', '"no_ads"', player?)
      www.youtube.com##+js(trusted-replace-xhr-response, /"adPlacements.*?("adSlots"|…)/gms, $1, /\/player(?:\?.+)?$/)

  Our surface is `tvhtml5`, and **it never requests `/player`.** Captured over CDP against the
  kiosk's own CEF profile, with a video cast to it exactly as a phone would, during a session
  where an ad *was* served (`pagead/adview` fired):
  - the watch data comes from **`/youtubei/v1/next`** (~475–517 KB) and contains **no** ad keys
    at all — not `adPlacements`, `playerAds`, `adSlots`, or anything matching `*[Aa]d*`;
  - `/youtubei/v1/browse` *does* carry `adSlotRenderer`, `adSlotMetadata`,
    `pageTopAdLayoutRenderer`, `adsControlFlowOpportunityReceivedCommand` — but those are the
    browse feed's *display* ads, not the in-stream one;
  - the ad-flow traffic is `static.doubleclick.net/instream/ad_status.js`,
    `googleads.g.doubleclick.net/pagead/id`, `pagead/1p-user-list`, `pagead/adview`, and the
    TV player bundle `tv-player-ias.vflset/tv-player-ias.js`.

  So subscribing to uBO would buy the *engine* and their generic scriptlets, but their
  YouTube rules would not match our surface — we would be authoring and maintaining
  TV-specific rules ourselves, which is the maintenance burden subscribing was meant to
  avoid. (`adblock` 0.13.2 does have the machinery, for whenever we want it:
  `Engine::url_cosmetic_resources` → `injected_script`, `use_resources`, a `PermissionMask`
  gating `trusted-*`, and a `resource_assembler` that reads uBO's repo directly. The missing
  piece is injection *timing* — scriptlets must hook `fetch`/XHR before page scripts, which
  needs a render-process `OnContextCreated` handler we do not have. Note uBO's scriptlet
  bodies are GPLv3: fetch them at runtime rather than vendoring.)

  **What is actually unresolved:** where the in-stream ad manifest reaches the TV player from.
  Not `next`, not a `/player` call. Answering that is RE work for `~/re-shell` (capture a
  session while an ad plays and follow `tv-player-ias.js`'s ad control flow), and it decides
  whether *any* filtering approach can work here or whether `skipAd` plus patience is the
  ceiling on this surface.
- **Q35 — SponsorBlock rate limits are unverified.** The API's own wiki is behind bot
  protection; the numbers that surfaced during research came from a *fork's* documentation
  and were not confirmed upstream. Today's usage is one hash-prefix lookup per video change,
  with no cache at all, which is almost certainly fine for one display but is not something
  we have checked. If it ever needs a cache: in-memory only, per the licence (segments on
  disk would be redistribution).
- **Q19 — cef/cef-binary version coupling.** `cef` crate 147.1.0 is pinned to nixpkgs cef-binary
  147.0.10. If nixpkgs bumps cef-binary, bump the crate pin (and archive.json is auto-derived from
  `pkgs.cef-binary.version`). A `nix flake update` could break the pair until re-matched.

## Bluetooth audio sink

Design settled in architecture-substrate.md §11 (own everything above HCI). **Q21–Q24 were
answered at the 2026-07-25 sync — all four confirmed as the proposed defaults.** Kept here
with their reasoning because each one is a behavioural commitment, not a structural one, and
the reasons are what a future reversal has to argue against.

- **Q21 — Dongle and firmware. ANSWERED: RTL8761BU, with our own uploader.**
  **AMENDED 2026-07-25: support a vendor registry, not a single chip.** Standardising on one
  part made the firmware uploader a hardcoding wearing a trait's clothes. Controller
  initialisation now sits behind its own `ControllerInit` seam with *two* implementations
  from the start — Realtek for the deploy dongle and Intel for the AX200 already in the dev
  box — because a seam with one implementation has never been tested as a seam. See
  architecture-substrate.md §11.3a. The AX200 also gives us a way to check the loader against
  the kernel's own: `btintel.c` is the spec, and `btmon` captures the reference transcript. The selection
  criterion was never antenna or spec version — it is **firmware**. Most modern controllers
  ship with no usable ROM image and depend on the OS driver uploading one at probe. Under
  Linux's `HCI_CHANNEL_USER` the kernel still runs the vendor `hdev->setup()` before handing
  over, but under WinUSB **nothing** loads it, so the chip's firmware protocol is ours.
  Standardising on RTL8761BU (TP-Link UB500 and the flood of identical cheap dongles) and
  writing the Realtek uploader — ROM version via vendor opcode `0xFC6D`, chunked download via
  `0xFC20`, ~150 lines, documented by `btrtl.c` — means both platforms use the *same* init
  path rather than depending on kernel behaviour on one and not the other. Buy 2–3 identical
  units so dev box and kiosk debug as one target.
  Rejected: CSR8510 (no firmware needed, but BT 4.0 only and the market is saturated with
  counterfeits), Intel AX200/AX210 (worst firmware flow of the lot).
  Practical notes for the build: BT spec version is irrelevant — A2DP is BR/EDR, and the 5.x
  additions are LE features. Some cheap "5.3" dongles are BLE-only and cannot do A2DP at all.
  Plug into **USB 2.0** and use a short extension cable: USB 3.0 radiated noise desensitises
  2.4 GHz radios and is the single biggest real-world range factor. And when Miracast lands,
  force its P2P group to 5 GHz or a mirroring session will stomp on A2DP (§7.5).
- **Q22 — LDAC decoder. REOPENED 2026-07-26: the premise was wrong, and the feature was a
  lie.** The original answer said LDAC has no libav support and AOSP's `libldac` is
  encoder-only, so decode means the reverse-engineered `libldacdec` over FFI. The first
  half holds; the second does not. nixpkgs ships **`ldacbt` 2.0.72**, an open-vela fork of
  Sony's own library, and it exports a complete decode API — not a reverse-engineered one:

  ```c
  HANDLE_LDAC_BT ldacBT_get_handle(void);
  int  ldacBT_init_handle_decode(HANDLE_LDAC_BT, int cm, int sf, int var0, int var1, int var2);
  int  ldacBT_decode(HANDLE_LDAC_BT, unsigned char *p_bs, unsigned char *p_pcm,
                     LDACBT_SMPL_FMT_T fmt, int bs_bytes, int *used_bytes, int *wrote_bytes);
  void ldacBT_free_handle(HANDLE_LDAC_BT);
  ```

  Two details make it a better fit than expected: `LDACBT_SMPL_FMT_F32` decodes straight to
  interleaved `f32`, which is exactly what `PcmBlock` holds — no conversion, and it avoids
  the planar/packed plane-length trap that silenced the right channel on every other codec
  — and `cm` takes the channel mode we already parse (`LDAC_CCI_MONO/DUAL_CHANNEL/STEREO`).
  The package ships no headers, so the signatures above come from the source
  (`inc/ldacBT.h`); do not write them from memory, an FFI signature is wrong silently.

  **Meanwhile the shipped behaviour was the exact failure this question exists to
  prevent.** `can_decode` answered `cfg!(feature = "ldac")` for LDAC, and the `ldac`
  feature is `ldac = ["audio"]` — it binds nothing. So a build with the feature on
  advertised an LDAC endpoint, a sender picked it, and `codec_id` refused every packet:
  a connected phone, a running session, and silence. Every build on 2026-07-25 had the
  feature on. Fixed by giving `can_decode` one source of truth — whether a decoder
  actually exists — so LDAC is simply not advertised today.

  **Parked, not blocked.** What it needs: `ldacbt` in the flake's build inputs and dev
  shell; an FFI module (the first non-ffmpeg decoder, so `unsafe` with `// SAFETY:` per
  ground rule 8); `AudioDecoder` refactored to a backend enum, since it currently *is* an
  `ffmpeg::decoder::Audio`; and `codec_id`/`can_decode` taught that LDAC has a decoder.
  Then the `ldac` feature means something and the endpoint can come back.
- **Q23 — Pairing and takeover. ANSWERED: Just Works, keys persisted, last-writer-wins.**
  `NoInputNoOutput` so neither side prompts, link keys persisted to the config dir so a
  repeat guest reconnects silently, and a second phone preempts the first to match the
  existing `SessionManager`. The accepted cost is that anyone in range can seize the
  speakers; the panel-confirmation alternative was rejected *for now* because it would
  block this work behind the touch UI, not because it is worse. Revisit once the panel UI
  exists.
  **AMENDED 2026-07-25: discoverable *always*, not "when idle".** The original wording said
  discoverable whenever no session is active. That is wrong for this box. The whole premise
  is that anyone in the room can walk up and throw media at the display, and the moment one
  person is connected is exactly the moment a second person wants to — a receiver that
  vanishes from every scan list while in use fails at its one job. Being non-discoverable
  during a session only ever helps a device that is *already* paired, which does not need
  discovery: it pages us, and that path is unaffected.
  This was found by testing, not by reading: an Android phone could not see the receiver at
  all while an iPhone was connected, and found it immediately once that link dropped. Note
  that nothing had ever called `set_discoverable` — the behaviour came from the radio, not
  from policy. The controller defaults to an 11.25 ms inquiry window every 1.28 s, under 1%
  of the radio, and an active A2DP link starves even that. So the box was advertising
  itself as discoverable and then failing to answer, which is worse than either choice: a
  user scanning sees nothing and gets no reason why.
  Bring-up now widens the inquiry scan to a 60 ms window every 640 ms — about 9% of the
  radio, spent deliberately — and turns on interlaced inquiry and page scan, which covers
  the frequency train in half the time. Interlaced is optional in the spec; a controller
  without it answers "unsupported feature" and bring-up carries on. `HostConfig` gained a
  plain `discoverable` flag in place of `discoverable_when_idle`, since the old name now
  describes behaviour we deliberately do not want.
  The cost is real and unmeasured: that 9% is taken from the same radio carrying the audio,
  in a room where 2.4 GHz is already contended, and it will compete again with Miracast's
  P2P group (§7.5). If streams start breaking up in a crowded room, this is the first knob
  to turn back down.
- **Q24 — Absolute volume. ANSWERED: the phone is authoritative.** We accept
  `SetAbsoluteVolume` from the phone and mirror it into the pipeline's gain; we push volume
  back only when the panel UI changes it. Matches what people expect of a Bluetooth speaker —
  the rocker in your hand works. Bidirectional sync was rejected: the phone's 0–127 scale and
  our 0.0–1.0 gain round differently, and two ends mirroring each other chase the rounding.

- **Q25 — The negotiated sample rate never reaches the decoder. FIXED.** Found on
  hardware: BlueZ negotiated **aptX at 48 kHz**, but `audio_session::run` built its
  decoder with `AudioStreamFormat::default()` — 44.1 kHz. aptX carries no in-band rate,
  so the decoder believes what it is told and the stream played ~9% slow, at the wrong
  pitch, with nothing in any log.
  Fixed as proposed: the format rides on the event, since it is a property of the session
  rather than of each frame. `castaway_core::AudioFormat` (non-zero rate and channel
  count) now travels `CodecCapability::format()` → `SinkEvent::Configured` →
  `SessionEvent::Audio { format }` → `Pipeline::play_audio(source, format)` →
  `audio_session::run`. The type deliberately has **no `Default`** — that impl was the
  bug, not the call site — so there is no longer a way to obtain a format without stating
  both numbers. The end-to-end adapter test now negotiates 48 kHz rather than 44.1 so a
  regression fails instead of coincidentally matching the old default.
  Also fixed alongside: a configuration that resolved to a rate but not to a single
  channel mode used to be accepted and then decoded as stereo; `CodecCapability::format()`
  requires both and the sink rejects with `INVALID_CODEC_PARAMETER` otherwise.
  **Confirmed on hardware 2026-07-26, from both ends independently.** Our path logs
  `stream configured codec=AptX format=48000 Hz × 2`, and BlueZ's own `MediaTransport1`
  `Configuration` property reads `4F 00 00 00 | 01 00 | 12` — Qualcomm, aptX, and `0x12`
  decoding to 48 kHz + stereo. The sender's view of the negotiation and ours now agree,
  which is precisely the property the bug consisted of violating.

- **Q26 — AVDTP START never arrives from BlueZ. RESOLVED ON HARDWARE 2026-07-26 — and the
  cause was not on the suspect list.** The rerun reached START, opened the session and
  played: `session: audio format=48000 Hz × 2`, source `bagel · aptX · 48 kHz · stereo`.
  **The stall was on the source side, not in our stack.** BlueZ's `MediaTransport1` sat at
  `State="idle"` — nothing had acquired it — while PipeWire dutifully streamed a full
  12-second tone into a node that was not routed anywhere. BlueZ only emits AVDTP START on
  transport acquire, so no START was ever sent, by anything, to anyone. Making the node the
  *default sink* took the transport to `active` and START arrived immediately. The original
  note described this exactly — "PipeWire happily streams five seconds into its node and
  nothing crosses" — which in hindsight is a statement about the rig, not a symptom of the
  receiver. Driving the bench needs the sink actually routed; `pw-play --target <node>` is
  not enough, and it fails silently by playing for exactly the right duration.
  **What the `btmon` capture disproves.** Suspect 1 is dead. On the media channel (CID 65)
  the configuration exchange completes cleanly in both directions, every time: their
  `Configure Request` (MTU 1021), our `Configure Request` (MTU 672), their
  `Configure Response Success`, and our `Configure Response Success` **arriving at BlueZ**.
  BlueZ then moves straight on to opening an SDP channel. Nothing is dropped, nothing is
  waiting, and there is no AVDTP Start anywhere in the trace to have been lost. Take the
  capture *first* next time — it cost an afternoon of reasoning that five minutes of trace
  refuted.
  **The ACL credit fix is real but did not cause this.** We genuinely never gated on
  credits: `HostAction::Credits` was emitted from `NumberOfCompletedPackets` and dropped on
  the floor, and `HostAction::Ready`'s `acl_credits` was ignored entirely. This dongle
  advertises **six** ACL buffers (`acl_credits=6 acl_mtu=1021`), so overrunning them under
  load is a real hazard and the spec forbids it outright. But the bench never came near the
  limit, and the fragment the theory said was discarded demonstrably arrived. Keep the fix
  on its own merits; do not let the commit message's confidence outlive the evidence.
  What that change bought is still worth having independently: one `AclWriter` task
  (architecture-substrate.md §11.3a-0) means the actor loop and the AVRCP control writer can
  no longer fragment onto one handle concurrently — which basic-mode L2CAP cannot recover
  from — and enqueueing never blocks, so the reader cannot be parked behind a write.

- **Q27 — Party mix: several senders at once. DEFERRED, and wanted.** Two phones connected
  simultaneously today and both streamed. The result was not a mix, it was two decoders
  fighting over one output device — fixed by making preemption actually work (Q23's
  last-writer-wins, which the pipelines had been ignoring). But the *idea* is a good one
  for this box: a room where two people can both throw audio at the display and get a
  blend is a better party than one where the second person silences the first.
  Deferred rather than rejected, because four things stand in the way and none of them is
  a tweak:
  - **Clock drift.** Two senders' 44.1 kHz are not the same 44.1 kHz. A mixer needs
    per-source resampling against the output clock, or the blend glitches periodically as
    the streams slide against each other. This is the real work.
  - **Volume authority.** Q24 makes the phone authoritative for absolute volume, which
    works precisely because there is one of them. Two phones both authoritative over one
    gain is a fight with no winner; mixing needs per-source gain and a decision about what
    the panel's volume then means.
  - **Metadata.** The now-playing surface assumes one source, and
    `SessionManager.active` is an `Option<SourceId>` — singular by design, not by
    accident. Two tracks playing needs a card that can say so.
  - **Transport control.** We publish an AVRCP Controller record and drive *the* sender.
    With two, the panel's pause button has no obvious target.
  So it is a session-model change plus a real mixer in the pipeline, not a flag. Worth
  doing as an explicit opt-in ("party mode") rather than a default: the failure mode of
  accidental mixing is that two people's music becomes nobody's, and in a hackerspace the
  person who did not expect it is the one who gets annoyed.

- **Q28 — The now-playing card never updates. OPEN.** It is a single snapshot taken when
  AVCTP connects, and nothing moves it afterwards. Skip a track and the screen keeps the
  old one; pause and the state never changes. Confirmed on hardware: one
  `GET_ELEMENT_ATTRIBUTES` request per session, one `NowPlaying` event, ever.
  **We register for no AVRCP notifications at all.** `avrcp::register_notification` and
  `avrcp::get_play_status` are both written and both unused; `pdu::PLAYBACK_STATUS_CHANGED`
  is defined and never referenced. The adapter's only outbound metadata traffic is that one
  attribute request. The comment beside it — "ask for metadata straight away rather than
  waiting for a notification; a track already playing produces no change event" — is
  justifying a belt-and-braces request alongside a subscription that was never written, so
  the design was half-finished rather than mis-decided.
  This is also why `state` reads `Stopped` on a playing stream: nothing populates it, so it
  is `PlaybackState::default()`. Not a mapping bug — an unasked question. (It happened to
  be correct in the run that surfaced it, because the phone really was paused, which is a
  good argument for not trusting a field nobody writes.)
  The fix is one mechanism for both. `RegisterNotification` answers INTERIM immediately
  with the current value and CHANGED when it moves, and is one-shot — re-register after
  each CHANGED. Subscribing to `PLAYBACK_STATUS_CHANGED` therefore yields the true state at
  connect *and* every transition, and `TRACK_CHANGED` says when to re-request attributes.
  Worth doing together with the cover-art fetch, which is stubbed at the same call site: we
  set `CONTROLLER_SUPPORTS_COVER_ART` in the SDP record and then only `debug!` the image
  handle a peer sends back, so album art is advertised and never fetched.

- **Q29 — Cover art needs L2CAP ERTM, and five smaller fixes. IMPLEMENTED; ERTM confirmed
  against the Linux kernel, the cover-art chain still unverified against a phone.** All six
  items landed, plus the AVRCP Target note at the end. The whole chain runs end to end in
  the tier-1 harness with no radio — SDP finds the image server past the browsing channel,
  the L2CAP channel comes up in ERTM, OBEX connects, attribute 8 is asked for *after* that,
  and the JPEG comes back through the retransmission engine onto the now-playing card.
  **What the virtual bench then proved, 2026-07-26.** `btvirt -l2` plus BlueZ's `l2test`,
  which is the kernel's own L2CAP driven as a peer — the Q13 pattern applied to a protocol
  rather than a codec, and now kept as `examples/ertm_echo.rs`:
  - **The mode is genuinely negotiated.** `l2test` reports `mode 3` — Enhanced
    Retransmission — and the trace shows the exchange landing where the spec says it
    should: their request carries TxWindow 63 / MaxTransmit 3 / retrans 2000 / monitor
    12000 / MPS 180, ours carries MTU 8192, TxWindow 32, **timeouts zero** (the requester
    does not get to pick those) and MPS 666, with a 16-bit FCS option.
  - **Both directions of segmentation and both directions of the checksum work.** Their
    MPS of 180 forced *us* to segment as well: three 800-byte SDUs arrived as
    Start/Continuation×3/End and reassembled, and our 600-byte echoes went back the same
    way and were delivered to their application intact. A wrong FCS, a wrong sequence
    number or a wrong SAR bit stalls that rather than degrading, and it did not stall.
  - **Our acknowledgements are accepted.** The trace shows RR with ReqSeq climbing 1..15
    and the piggybacked ReqSeq on our own I-frames tracking their sequence correctly.
  - **The interop risk is settled, at least for BlueZ.** It proposes
    `Mode: Basic (0x00)` *explicitly* on the AVDTP channel — an RFC option naming basic
    rather than an absent one — and we accept it and carry on to a full AVDTP capability
    exchange. All four codec endpoints came back and BlueZ parsed every one of them
    (aptX HD, aptX, AAC, SBC with the right vendor ids and parameters), which is also the
    first time `codec.rs` has been marked by someone else's parser.
  What the bench cannot show is the cover-art chain itself: BlueZ as a source serves no
  image server, so OBEX-over-ERTM against a *phone* is still unproven. **Take that capture
  before believing the artwork works.**
  **One bug the bench found in five seconds**, which no scripted test could have: btvirt
  answers `WriteInquiryScanType` with a command *status* of "unknown HCI command" rather
  than a completion, and bring-up stopped dead there — no `Ready`, no `WriteScanEnable`, a
  receiver nobody can find, and nothing in the log to say why. The comment beside that
  command already claimed a controller without it "carries on regardless"; it did not.
  A refusal now advances the queue exactly as a completion does.
  What each item turned out to be, now that the sources have been read rather than
  recalled:
  - **The feature bit is confirmed wrong, and the replacement confirmed right.** BlueZ's
    `profiles/audio/avrcp.c` has `AVRCP_FEATURE_BROWSING 0x0040` — bit 6, exactly what we
    were setting for cover art — and `CT_GET_IMAGE_PROP 0x0080` / `CT_GET_IMAGE 0x0100` /
    `CT_GET_THUMBNAIL 0x0200` at bits 7, 8 and 9. The Controller and Target records do not
    share a layout past the categories: the Target's cover-art bit is `0x0100`, which is
    `GetImage` in a Controller record. We claim bit 9 only, since the thumbnail is the one
    operation we know how to ask for.
  - **`x-bt/img-thm` and Img-Handle `0x30` both confirmed** against `obexd/client/bip.c`.
    The handle header is the subtle one: `Name` and `Img-Handle` are both length-prefixed
    UTF-16 headers, so putting the handle in `Name` encodes perfectly, reads back
    perfectly, and produces a GET that named no image at all.
  - **The PSM selection is the same test BlueZ makes** — walk the additional descriptor
    list for the stack containing an OBEX UUID, then take *that* stack's L2CAP port.
  - **ERTM converges only if the negotiation is asymmetric.** Both ends propose at once,
    and the first implementation had both sides adopting whatever the other asked for,
    which swaps modes forever and never opens the channel. The rule that works: the side
    that listened holds the mode its service was registered with, the side that dialled
    adapts. Audio is registered basic, so a sender that now sees the ERTM bit and proposes
    it for AVDTP gets a counter-proposal rather than a refusal.
  - **Advertising ERTM is not free**, and this is the interop risk to watch on the bench:
    the extended-features mask now says bit 3, so senders that previously did not bother
    may start proposing ERTM for the *audio* channels. They should fall back — AOSP and
    BlueZ both do — but "should" is the word that cost Q26 an afternoon.
  Original research below, kept because the reasoning is what made the fixes findable.
  Researched after an iPhone streamed happily and never sent an image handle. The easy
  conclusion — "iOS does not do AVRCP cover art" — is **wrong**, and worth recording as
  wrong because it is the third time today that "the peer does not support it" turned out
  to be our own bug wearing a disguise (see the aptX HD vendor id).
  The BlueZ cover-art patch series (Frédéric Danis, Collabora, linux-bluetooth 2024-09-17)
  states it was tested against **iPhone 14, iPhone 15 Pro and Samsung S23**. BlueZ is an
  uncertified non-MFi controller with no Apple auth coprocessor, so modern iPhones do
  publish the OBEX PSM and serve BIP to anyone who asks correctly. Apple's own docs
  contradict each other — the consumer page claims AVRCP 1.6 with album art, while the MFi
  Accessory Design Guidelines say AVRCP 1.4 and never mention cover art, routing artwork
  through iAP2 — which is where the folklore comes from. iAP2 and CarPlay are separate
  mechanisms we cannot and need not use.
  **The blocker was ours.** AVRCP 1.6.3 §14 requires GOEP 2.0 for cover art transfer, and
  GOEP §7.1.2 requires the OBEX channel to be configured for **Enhanced Retransmission
  Mode**. `substrate-l2cap` was basic mode only: it answered the extended-features
  InformationRequest with a zero mask and refused by name any non-ignorable config option
  it did not implement. So when a peer tried to configure the cover-art channel for ERTM
  we rejected it, and every other fix below only got us as far as that refusal. This also
  explains Apple Developer Forums 786623, where an iPhone SE advertises Cover Art, OBEX
  CONNECT gets no answer, and "l2cap s-frame response always failed" — S-frames are ERTM.
  Ordered by dependency, with what each became:
  1. **ERTM in `substrate-l2cap`** — the hard one, and it gated the rest. Landed as
     `ertm.rs`: control field, SAR, CRC-16 FCS over the basic header, REJ/SREJ/RNR, the
     send window, and the poll ⇄ final exchange, with time passed in by `tick` rather than
     read from a clock so the retransmission path is a unit test.
  2. `record.rs` — done, bit 9, and bit 6 is now named `CONTROLLER_SUPPORTS_BROWSING` so
     the next person cannot set it by accident.
  3. `record.rs` — done, the Controller record lists `0x110E` and `0x110F`.
  4. `adapter.rs` — done, and it made the OBEX client a *session*: opened when AVCTP
     connects, held across tracks, with the metadata asked for twice — the text
     immediately, so the card does not wait on an SDP query and a second channel, then the
     full set once BIP is up.
  5. `obex.rs` — done, both halves.
  6. `client.rs::cover_art_psm` — done, via `ServiceRecord::l2cap_psm_under`.
  Also noted, unrelated to fetching: our AVRCP **Target** side should tolerate attribute 8
  in an inbound GetElementAttributes and skip ids it does not know rather than reject the
  PDU — real GM and Hyundai-Kia head units enumerate 1..=8 unconditionally. **Done, and it
  was worse than "should".** Nothing separated commands from responses, so an inbound
  GetElementAttributes *command* was parsed as a response — its eight-byte track
  identifier reading as an attribute count of zero — and a head unit asking us what was
  playing silently emptied the card the phone had just filled in.
  **Still open: the capture.** No public iPhone `0x110C` SDP dump appears to exist
  anywhere. Taking one in `~/re-shell` (BlueZ >= 5.81, `bluetoothd --experimental`,
  `mpris-proxy`, `btmon`) would give both a golden SDP record and a live attribute-8
  response — fixtures that do not currently exist publicly, which is exactly what rule 9
  asks for, and the only thing that will confirm any of the above. What to look for, in
  order: does the phone's extended-features response come back with ERTM set; does our
  configuration request for the image PSM get Success rather than a counter-proposal; does
  OBEX CONNECT get an answer; and does the metadata response *after* that answer carry
  attribute 8 when the one before it did not.

- **Q30 — Tapping the composited output: screenshots now, HLS/DASH later. PHASE 1 LANDED.**
  Two wants with one root: a screenshot endpoint (so the panel can be inspected remotely,
  and so anyone working on a surface can see it without standing in front of the display),
  and a duplicate of the output as a web stream. Both need the same thing — a way to read
  back what the compositor just drew.
  **wgpu cannot encode.** It is a WebGPU implementation: graphics and compute, no video
  encode or decode surface, and none in the spec — browsers put that in WebCodecs, which
  is a different API. So there is no "NVENC-style encoded readback" to ask wgpu for. An
  encoded tap has to reach the vendor encoder directly.
  The good news is that this codebase already does that interop, in the other direction:
  `pipeline/src/hwaccel` imports decoded hardware frames zero-copy — VA-API → dma-buf →
  Vulkan external memory on Linux (`dmabuf.rs`, `vulkan_import.rs`), D3D11VA → shared
  handle → DX12 on Windows (`d3d11va.rs`, `dx12_import.rs`). Encode is that run backwards:
  pull the native handle out of the wgpu texture (`wgpu-hal`'s `as_hal`), export it, hand
  it to ffmpeg's `h264_vaapi` / `h264_nvenc` / `h264_amf`. ffmpeg is already linked.
  Three things stop it being a mirror image, and they are the real work:
  - **Colour.** Encoders want NV12; the compositor renders RGBA. Converting on the CPU via
    swscale would waste the entire exercise. It belongs in a wgpu pass writing NV12 planes
    — and the compositor already does YUV→RGB in its shader, so the inverse is
    well-trodden here.
  - **NVENC is not the VA-API path.** On NVIDIA/Linux `h264_nvenc` wants a CUDA or D3D11
    frames context, so it is `VK_EXT_external_memory_fd` → `cuImportExternalMemory`. A
    third interop path, not a reuse of the first.
  - **A stream has its own clock.** The panel presents at display refresh; HLS wants a
    steady 30/60 with monotonic PTS. And ground rule 4's drop-late-frames rule inverts: a
    stream cannot drop without a gap, so it duplicates.
  **Phase 1, done:** an `OutputTap` seam plus a CPU-readback screenshot. The trait asks
  `wants_frame` *before* the readback, so a tap that declines costs nothing and no taps
  cost nothing at all — a 4K RGBA copy is 33 MB and must never be on the default path.
  One readback per frame is shared by every tap that wanted it.
  **Phase 2, open:** an encoder tap. The frame it receives is deliberately shaped like the
  existing `FrameImage::{Cpu, Gpu}` split, so the zero-copy version is a variant rather
  than a redesign of the seam.

- **Q31 — The display sleeping takes the audio sink with it. OPEN.** Observed on the dev
  box: the panel went into DPMS sleep, and within seconds the GPU's audio codec reported
  `monitor_present 0` / `eld_valid 0` on every ELD, PipeWire removed the
  `Navi 31 HDMI/DP Audio` sink, and everything fell back to `Dummy Output`. The DRM
  connector still read `connected` throughout, so nothing looked wrong from that angle.
  `kscreen-doctor --dpms on` brought the monitor back and the sink returned immediately.
  This is a real failure mode for the product, not a dev-box quirk. The deploy target is a
  wall-mounted panel whose audio *is* the HDMI endpoint, and the most common session —
  Bluetooth — has no pixels of its own. So the natural sequence is: someone connects a
  phone, music plays, nobody touches the screen, the panel sleeps, and the music stops.
  Two things to settle, and they are separable:
  1. **Keep the panel awake while a session is active.** This is exactly what
     `control-display` is for, and it is currently the `NullDisplay`: the session manager
     already calls `power_on` and `select_input` on session start and the log dutifully
     says `display: power on (null)` while nothing happens. Wiring a real DDC/CEC backend
     is the fix; the seam is already there and already called.
  2. **Survive the sink disappearing anyway.** Even with (1), a panel can be switched off
     at the wall or swapped inputs. Unverified: what our audio session does when its cpal
     output device vanishes mid-stream. The likely answer is that the stream errors, the
     session logs "output failed" once, and the phone keeps streaming happily into a
     decoder writing nowhere — silence with a connected phone and a now-playing card still
     on screen, which is the worst of the available outcomes. Wants a test that pulls the
     device out from under a running session.
  Also worth noting for whoever wires (1): the sink is *removed* and later *re-added* as a
  new node, so "reconnect to the same device" is not a thing — the audio path has to be
  re-established against whatever the default is when it comes back.
