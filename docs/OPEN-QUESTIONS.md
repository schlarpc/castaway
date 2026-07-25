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
  been compile-checked, and decode is software-only (hwaccel is Q20). Both want the
  C6522QT box.
- **Q20 — Hardware-accelerated decode.** Software decode is fine for the 1080p test
  streams and will not hold a 4K60 mirror on the deploy box, so this is scoped rather
  than speculative. The framing question is *not* "turn on VAAPI" — it is "who owns the
  decoded surface", and that answer changes types in `core`.

  Findings that shape it:
  - **`ffmpeg-next` 7.1 wraps none of this.** No `hw_device_ctx`, no `get_format`, no
    `AVHWFramesContext` anywhere in the crate — every piece is raw `ffmpeg_sys_next`
    through `codec::Context::as_mut_ptr()`. Ground rule 8 permits that in `pipeline`, but
    it means `// SAFETY:` comments and a thin safe wrapper at the crate boundary.
  - **The naive version is a regression.** Decoding to a GPU surface and then
    `av_hwframe_transfer_data`-ing back to system memory for the existing swscale → RGBA →
    `queue.write_texture` path trades CPU decode cycles for a GPU→CPU→GPU round trip; at
    4K the readback usually costs more than it saved. Hwaccel only pays if the surface
    never leaves the GPU, so this is a zero-copy *import* project, not a decode project.
  - **wgpu 22 already does the sampling half.** `Features::TEXTURE_FORMAT_NV12` plus
    `TextureAspect::Plane0`/`Plane1` views works on Vulkan and DX12. Only the import needs
    raw `wgpu-hal`.
  - **Linux is not hardware-blocked.** `av_hwframe_map` to `AV_PIX_FMT_DRM_PRIME` yields an
    `AVDRMFrameDescriptor` (per-plane fds + DRM format modifier) without calling libva
    directly; import via `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`
    and `wgpu_hal::vulkan::Device::texture_from_raw`. The dev box's RX 7900 XTX (RADV) can
    verify all of that natively by offscreen readback.
  - **Windows costs one GPU-local copy.** D3D11VA hands back an `ID3D11Texture2D` + array
    index, but wgpu runs DX12/Vulkan there, and ffmpeg allocates its decoder array with
    `BIND_DECODER`, which is generally not shareable. Realistically: `CopySubresourceRegion`
    into a shared NV12 texture we own, `IDXGIResource1::CreateSharedHandle` →
    `ID3D12Device::OpenSharedHandle`. Still far cheaper than a readback, but not literally
    zero-copy. Only this step needs the Dell.
  - **Colorspace stops being swscale's problem.** Surfaces are NV12 (P010 for 10-bit), so
    YUV→RGB moves into the fragment shader and `AVFrame.colorspace`/`color_range` (BT.709
    vs 601 vs 2020, limited vs full) must reach the compositor. `DecodedFrame` carries
    neither today. Getting it wrong is subtly wrong — washed out or oversaturated — not
    obviously broken, which is the worse failure.
  - **Fallback is most of the correctness.** Hwaccel fails routinely in the field:
    unsupported profile, too many reference frames, 10-bit on an older GPU, a VM with no
    device. Decode must fall back to software *mid-session* without dropping the mirror.

  Default, unless you say otherwise: the hw/sw choice is a **runtime** decision behind a
  `HwDecodeBackend` trait, not a cargo feature — a `--features vaapi` that turns a working
  mirror into a black screen on the wrong GPU is exactly the failure mode to avoid. The
  feature flag gates whether the backend is *compiled*, never whether it is *used*.

  Slicing, so the portable-crate churn happens once and early:
  1. `DecodedFrame` gains a GPU variant and colorspace metadata; `HwDecodeBackend` lands
     with a null impl. Nothing gets faster, the shape gets right. (Touches every consumer —
     do it deliberately, per ground rule 5.)
  2. VAAPI → DMA-BUF → `wgpu-hal` Vulkan import, proven by offscreen readback on the dev
     box. This is the slice that proves zero-copy end to end.
  3. NV12 shader sampling replaces swscale on that path.
  4. D3D11VA + shared-handle bridge, on the Dell.

  Steps 1–3 are natively verifiable here; only 4 is hardware-gated. `get_format` selection,
  the fallback state machine, and colorspace matrix derivation are pure functions over
  metadata and get unit tests regardless of GPU (ground rule 6). Mirroring also wants
  `AV_CODEC_FLAG_LOW_DELAY` and no frame-level threading — hwaccel decoders will otherwise
  buffer 2–3 frames, which is the wrong trade for a live mirror.

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
- **Q19 — cef/cef-binary version coupling.** `cef` crate 147.1.0 is pinned to nixpkgs cef-binary
  147.0.10. If nixpkgs bumps cef-binary, bump the crate pin (and archive.json is auto-derived from
  `pkgs.cef-binary.version`). A `nix flake update` could break the pair until re-matched.
