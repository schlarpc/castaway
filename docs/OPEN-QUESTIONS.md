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

- **Q12 — Cast RTP receive + frame reassembly.** Negotiation + AES-CTR decrypt are done;
  the UDP RTP receiver (Cast's own RTP framing: frame_id, packet_id, reference frames,
  RTCP feedback/NACK) that assembles encoded access units is not. Needs an openscreen
  `standalone_receiver` capture to build against. This is the bulk of the media plane.
- **Q13 — Per-frame IV derivation.** `mirror::frame_iv` XORs the frame_id into the low 4
  bytes of the IV mask. This is self-consistent but not verified against openscreen's
  exact scheme — confirm from a capture before trusting decrypted output.

## App / hardware wiring

- **Q14 — Dell C6522QT RS-232 opcodes.** `control-display::dell` models the command frame
  (header/id/category/opcode/len/data/XOR-checksum) but the opcode bytes (power, input
  select) are placeholders. Confirm against Dell's C6522QT *RS232 External Control
  Application* manual before trusting on hardware.
- **Q15 — Cast TLS actor + AirPlay RTSP actor.** The pure cores (`proto-cast::CastSession`,
  `proto-airplay::AirPlaySession`) are done and tested, but the socket actors that own the
  connection (CASTv2 needs a TLS server with a self-signed cert the device-auth signs over;
  AirPlay needs the RTSP TCP server + the post-pairing ChaCha20 `ByteTransform`) aren't
  written. Until they are, `app` keeps Cast/AirPlay mDNS ads OFF by default (D16). These
  actors are the main remaining "make it connect" work for those two protocols.
- **Q16 — Real pipeline behind features.** `NullPipeline` proves the stack; the ffmpeg
  decode + wgpu compositor + winit kiosk surface (and CEF) are declared feature flags with
  trait surfaces (`Compositor`, `BrowserSurface`) but no backend impls yet. Wiring these is
  the render-path milestone (needs the C6522QT box for real validation).

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
