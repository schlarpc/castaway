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
- **Q19 — cef/cef-binary version coupling.** `cef` crate 147.1.0 is pinned to nixpkgs cef-binary
  147.0.10. If nixpkgs bumps cef-binary, bump the crate pin (and archive.json is auto-derived from
  `pkgs.cef-binary.version`). A `nix flake update` could break the pair until re-matched.
