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
| `pipeline` | Null backend live; `Compositor`/`BrowserSurface` traits. ffmpeg/wgpu/cef = feature stubs (Q16). |
| `control-display` | Null backend + Dell RS-232 frame encoder (opcodes placeholder, Q14). |
| `input-touch` | `TouchSource` trait + null; evdev/winuser feature stubs. |
| `app` | **Runs.** One HTTP host (DLNA+Spotify+DIAL) + one SSDP + one mDNS + session mgr. TOML config. |

## Verified working end-to-end (curl against `cargo run`)
- DLNA description + SOAP (`GetTransportInfo` → `NO_MEDIA_PRESENT`).
- Spotify `getInfo` (returns DH public key), advertised on mDNS.
- DIAL `dd.xml` with correct `Application-URL` header; launch flips state to running.

## Biggest open items (see OPEN-QUESTIONS.md)
1. **Q15** — Cast TLS actor + AirPlay RTSP actor (the "make it connect" work; pure cores ready).
2. **Q1** — FairPlay-SAP + AirPlay pairing captures (gates AirPlay mirroring).
3. **Q16** — real pipeline (ffmpeg → wgpu → kiosk) behind the feature flags.
4. **Q2/Q11** — real Cast device cert for Chrome to accept auth.
5. **Q12/Q13** — Cast mirroring RTP receive loop + IV validation.

## Design decisions worth your review
D7 (router composition vs SourceAdapter), D9 (hand-written prost, no protoc), D10 (Spotify
scope), D16 (socket protocols advertise-gated). All in DECISION-LOG.md.
