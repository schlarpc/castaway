# Build Status — begun 2026-07-23, brought current 2026-08-08

Snapshot for our next sync. Companion to DECISION-LOG.md (why) and the issue tracker
(what needs you; `docs/OPEN-QUESTIONS.md` was migrated there and deleted — see #105).
Everything below builds with `cargo build`, passes `cargo nextest run` (**2397 tests** as of
2026-08-08 — every feature is on by default since D55), passes `cargo clippy --all-targets -- -D warnings`,
`nix build` produces a running binary, and `nix flake check` passes — 20 checks, which
include the VM tests (integration, miracast, bluetooth, gamestream, matter, mixer, dial), the
two Android-emulator checks (`android-bt`, #225 — a real phone stack pairs, registers
volume, streams aptX HD, and the waveform is correlated per channel; and `android-cast`,
#225 — the same phone on a TAP segment, where **Play Services' own Cast picker lists the
panel, passes device auth against it, and mirrors its screen**, with the RTP counted off
the wire), the hosted
Cast-app check (`cast-app-hosting`, #16 — both receiver-SDK generations against the real
Electron runtime), the openscreen differential checks, the bindings guards
(`moonlight-bindings`, `ldac-bindings`), and the Windows cross-build DLL-closure check.

**What a green check does and does not mean is recorded in `docs/test-matrix.md`** — a
per-mode audit of what each test actually asserts. It found 140 tests that no check compiled,
a build configuration broken on `main`, and several claims in this file that had gone stale;
the first two are fixed (D55, #180, #182) and the third is corrected here. The old summary —
"no VM test asserts decoded media content" — is no longer true for audio: `bluetooth-vm`
correlates a decoded chirp per window (#186) and `android-bt` correlates VLC's waveform per
channel (#225). Video is still the gap: no VM test decodes a picture, and for the mirroring
protocols the strongest "it works" is still a journal line or an access-unit count —
`android-cast` moves that floor for Cast without lifting it, by counting the phone's RTP
out of the segment capture rather than trusting the journal that a mirror was negotiated.

## What exists (workspace per architecture-substrate.md §2)

The table below describes 24 crates; the workspace now has 32. The Bluetooth stack
(`proto-bluetooth-audio`, `substrate-hci`, `substrate-l2cap`, `substrate-sdp`,
`hci-transport`, `ldac-sys`), `crypto-playfair`, `cast-replay`, `cast-registry`, `paths`
and `sponsorblock` arrived after it was written and are covered in the sections further
down (or, for `cast-registry`, inside the `proto-cast` row) rather than here.
`vendor/mdns-sd` is not a crate of ours but rides in-tree: a patched fork that can emit
one PTR per DNS-SD sub-type (#227), applied via `[patch.crates-io]`, meant to be offered
upstream and then deleted.

| Crate | State |
|---|---|
| `core` | Done. Traits (`SourceAdapter`, `Pipeline`, `DisplayControl`, `MiracastBackend`), `SessionEvent`/`FrameSource`, newtypes, last-writer-wins `SessionManager`. |
| `test-support` | **Done.** The shared deadline poll (`eventually` and its async/chosen-deadline spellings, #236) — a dev-dependency everywhere, a runtime dependency nowhere. |
| `paths` | **Done.** The one answer to "which directory?" on both platforms (D39). `Layout::{Xdg, LocalAppData}` is a value, not a `cfg`, so Linux CI exercises the Windows layout — absoluteness rule included. Resolution is pure (an `Environment` trait, no process env, no disk); finding nothing is reported as `Origin::Fallback` rather than guessed at. |
| `substrate-ssdp` | Done. Pure M-SEARCH/NOTIFY layer + UDP 1900 `Responder`. |
| `substrate-mdns` | Done. `mdns-sd` wrapper, validated `MdnsService`. |
| `substrate-rtsp` | Done. `rtsp-types` framing + CSeq + `ByteTransform` slot + AirPlay's bare-path request-URIs. |
| `substrate-rtp` | Done. RFC 3550 parse + gap-skipping reorder buffer. |
| `crypto-cast-auth` | Done. RSA device-auth signer (SHA1/256, PKCS1v15). |
| `crypto-raop` | **Done.** The two RSA operations AirPlay 1 needs — OAEP/SHA-1 to unwrap `a=rsaaeskey:`, PKCS#1 v1.5 unprefixed to sign an `Apple-Challenge` — over the published AirPort Express key, quarantined in one crate. |
| `crypto-fairplay` | **Handshake done.** Both `/fp-setup` round trips answer correctly — SETUP1 is a table lookup on byte 14, SETUP2 a header plus a 20-byte echo with no crypto in it. The one boundary left is `decrypt_ekey` (the `ekey`→AES unwrap), which needs the OmgHax tables and a licence decision, not a capture (#39). Gates mirroring only. |
| `proto-dlna` | **Live.** AVTransport/RenderingControl/ConnectionManager SOAP; cast-a-video works. |
| `proto-cast` | **Live, all three paths.** Framing, JSON, device-auth, media LOAD, and a TLS actor on 8009 driven end-to-end in the VM test. Mirroring is complete: OFFER/ANSWER negotiation, RTP reassembly, RTCP feedback, AES-CTR decrypt, and a UDP actor — differential-tested against openscreen's own packetizer (#53/#54). **App hosting is real (#16):** a `LAUNCH` for a page app resolves through `cast-registry` (app id → receiver page, against Google's public registry, with a cache so a stale entry beats a failed fetch), opens in the Electron browser, and the sender is answered when the page's own Cast Receiver SDK dials back through the loopback-only platform channel — `checks.cast-app-hosting` drives both SDK generations and a hosted app playing real media. A build with no browser still declines rather than fakes, and `GET_APP_AVAILABILITY` is answered (D32). **A real Play Services sender now lists the panel** (#226): the picker matches on DNS-SD sub-types and nothing else, so one is published per natively served app id (through the vendored `mdns-sd` fork, #227), the DeviceProber's `GET_DEVICE_INFO` and `eureka_info` are answered, and the TXT record is a typed vocabulary. That closed the distance #40's theories claimed: the CRL was measured 2026-08-08 and revokes neither identity (`cargo run -p cast-replay --example crl_check`), and the "trust root a real sender would refuse" is the identity in the picker. Device auth remains differential-tested against openscreen's *sender* (D31). The two borrowed identities are carved from their sources at build time — no credential material rides in the tree (#154) — each with a live endpoint behind it, so a revocation is a config edit rather than a dead panel (D41, D43, D44). `cast-replay` owns both. **And a real Play Services sender now mirrors to it** (#225): `checks.android-cast` drives Android's own Cast picker over a TAP segment — the picker lists the panel, device auth passes against real Play Services, the OFFER/ANSWER completes, and the phone's RTP is counted off the wire rather than taken from our journal. That was #226's residual. **Not done:** no real sender has launched a *hosted page* app end to end (#228) — the mirroring receiver is a native app id, which is a different path. |
| `proto-spotify` | **Live, pairing through playback, with shuffle/repeat both ways.** Ours: advertise + `getInfo` + `addUser` DH/blob decrypt, on the shared HTTP host and mDNS responder. librespot's, above the LAN: AP login, dealer, connect-state, audio (D30). Pick castaway in the Spotify app and it logs in as you, plays, honours the phone's transport and queue, and drives back from the panel via `RemoteControl`. No account on disk. Blob framing still unproven against a real phone (#48). Queue and cover art now render (#49/#50). |
| `crypto-playfair` | **Done, and vector-verified.** The FairPlay v3 `ekey`→AES unwrap that gates mirroring. Tables generated and `garble` transpiled from the published source rather than retyped; correctness settled by the twenty published vectors, all four modes. Own crate because the material is GPL where this workspace is MIT. |
| `proto-airplay` | **AirPlay 1 audio *and* mirroring, both end to end.** `FLUSH` discards what a seek left behind. `ANNOUNCE` → `SETUP` → `RECORD` as a state machine that carries what each step settled, three UDP sockets bound *before* `SETUP` answers, AES-CBC payload decrypt, and frames into the pipeline — driven over real sockets by an integration test. `et=0,1`: the RSA session-key unwrap works (`crypto-raop`), and `Apple-Challenge` is signed. `SET_PARAMETER` volume/progress/DMAP metadata reach the now-playing card, which also shows the negotiated generation and codec. Timing probes and resend requests are sent, not just parsed. Mirroring: the two-phase `SETUP` plist, FairPlay unwrap, SHA-512 stream keys, the 128-byte-framed data channel with its continuous AES-CTR keystream, AVCC→Annex-B with in-band SPS/PPS — driven from a real FairPlay vector over real sockets in an integration test. **Instrumented for a device session**: every live session logs one structured line every 5 s — frames and drops per plane, resends, stale/unanchored discards, the clock offset and delay, the latency the sender declares, and `av_skew_ms`, the video-minus-audio presentation difference that *is* the lip-sync error. Mirror audio (AAC-ELD) rides the same session, keyed by the same FairPlay key with the `eiv` verbatim, through the same depacketiser. **Since then:** `FLUSH` honours `RTP-Info` (#120); the **video path exists** (#80) — `/play` hands over a URL, `/scrub`/`/rate`/`/playback-info` drive it, and `/reverse` answers `101 Upgrade: PTTH/1.0` with nothing yet pushed over it; a mirror also ends when its data channel closes (the Control-Center stop); mirrored frames carry the *negotiated* codec instead of a hardcoded H264 (#118/#119); and the decode seam is proven — real two-tone ALAC across a fully negotiated session decodes bit-identically on the clear and the encrypted legs (#189), which found the 2 KB audio-socket read that truncated any 4096-sample SDP frame into static under a green journal; and the mirror's video leg is proven the same way (#281) — a real x264 IDR+P pair through the full FairPlay-negotiated session decodes via `pipeline::ffmpeg_decode` to pictures of the negotiated geometry within a mean pixel error of ~1.4 of the encoder's input. Mirror audio's AAC-ELD *decoder-open* is pinned (the AirPlay `AudioSpecificConfig` opens, a truncated object type is refused), and the pinned ffmpeg demonstrably decodes real ELD — but it cannot *encode* it (the native `aac` encoder accepts `-profile:a aac_eld` and writes LC frames behind a corrupt ASC), so the ELD tone-through fixture still needs a capture from a real Apple sender (#281's residual). **Not done:** no clock discipline beyond the pipeline's own pacing, and no presentation clock aligning the mirror's two planes — both stamp against one origin (real since #152), so the offset is *measurable*, but nothing corrects it yet. HEVC is implemented behind `[airplay] offer_hevc`, off by default. AirPlay 2 is untouched — see `docs/airplay-research.md` §2.2. |
| `sponsorblock` | **Live.** Hash-prefix lookup, category/overlap filtering, and the when-to-skip planner — pure, fixture-tested. Driven by an actor in `app` that binds to our own screen as a Lounge remote. |
| `proto-dial` | **Live launch, and a phone really plays through it** (`yt-selfplay`), including the attach-to-a-running-app path via a published `<screenId>`. Gated on a launch target: a build with no browser does not advertise DIAL. Pure Lounge bind-channel parser/mapping kept for a browser-less fallback; no native Lounge client. |
| `proto-miracast` | **The whole protocol, none of the radio.** The WFD information element (byte-identical to what MiracleCast and lazycast put on the air), the `wfd-kv` parameter language as one type per parameter, the M1–M16 exchange with its two independent CSeq counters, MPEG2-TS-over-RTP demuxed to `EncodedFrame`, UIBC touch/HIDC encoding with a coordinate type that cannot carry panel pixels, and a tokio actor driven end-to-end by a scripted source over real sockets. AOSP's format chooser is reimplemented as an oracle, so what an Android phone *would* pick is asserted in tests rather than guessed. On by default in `app`; a radio that cannot be group owner logs and skips, and `[enable] miracast = false` is for boxes whose uplink *is* that radio. **And now the radio, minus the driver.** The Linux backend forms a real autonomous P2P group in CI (`checks.miracast-vm`, mac80211_hwsim): a second radio in its own network namespace discovers the sink's WFD IE over the air, joins by WPS push-button, takes a DHCP lease from the group the NixOS module serves, and the sink resolves the peer with its own neighbour-table sweep (in WFD the peer never speaks first), dials its 7236, negotiates M1→M7 to a running mirror, receives hand-rolled MPEG2-TS-over-RTP across the group — PAT, PMT, and a PES the pipeline counts as *encoded*-plane access units, proving the advertised RTP port is really reachable (the §7.2 two-minute-watchdog failure) — and ends with a clean triggered teardown. (The frame count is `frames=[1-9]`, a completed-access-unit counter, not a decoded picture: the payload is NAL stubs, the keyframe flag is not asserted despite the test's own comment, and nothing decodes it. See `docs/test-matrix.md` §4.6.) The backend speaks to all three of wpa_supplicant's control surfaces — the `p2p-dev-*` management socket that actually delivers P2P events, the group interface's socket where the WPS registrar lives, and an abstract-namespace reply path that survives `PrivateTmp=`. **And now the other way in.** Miracast over Infrastructure ([MS-MICE], #166) runs the same RTSP session over the ordinary WLAN instead of a P2P group — a `_display._tcp` mDNS registration through the shared responder, a control channel on TCP 7250, and a `SOURCE_READY` that says where to dial. Windows 10 v1703+ *prefers* it, and it is the one Miracast path that needs no P2P radio at all. The control protocol is pure and driven from [MS-MICE] §4's own golden fixtures, its PIN hash from §3.1.5.6.1's two vectors, and the hand-off from a scripted source over real sockets. The DTLS and PIN flows are neither advertised (`Capability::insecure` is the spec's own `0x05`) nor served: a source that asks anyway is refused in one message rather than left to time out. On by default; `[miracast] infrastructure = false` turns it off. **Since then:** the M13 IDR request — WFD's only loss recovery — fires and stops firing at three tiers, ending with `miracast-vm` dropping two datagrams on a schedule and asserting `lost=2`/`video_gaps=2`/`idr_requests=3`, the rate limit owned by the session as arithmetic (#192, #235); the UIBC back-channel's socket half is tested in the *source's* pixel space, and `wfd_uibc_setting: disable` revokes the touch surface instead of stranding it (#125/#193); a source that offers HDCP is **refused** with a typed error naming the value, and an idle session is torn down by a liveness watchdog (#195); audio and video each wrap on their own 33-bit PTS counter, so a sender whose uptime crosses 26.5 h mid-cast no longer explodes both planes (#231); and the audio plane's continuity gaps are counted (#233). **Not done:** any *real* driver — hwsim is the best-behaved mac80211 there is, and the §7.6 driver check and 5 GHz both remain the hardware's to pass (#17); no MICE session has run against a real Windows source, and the MICE beacon extension turned out to be a WPS attribute inside the WSC IE that wpa_supplicant 2.11 writes only over D-Bus, so it is still not installed (#194); and nothing decodes the received TS — the VM's frame count is still an access-unit counter. **And settled, negatively:** there will be no Windows radio backend. The §7.7(c) spike (`examples/wfd-probe.rs`, run 2026-08-01) stands up an autonomous group owner from an unpackaged Win32 process with no capability at all, and then `Start()` **aborts** the moment the advertisement carries the WFA OUI — the gate is `50:6f:9a` itself, not the OUI type — so a third-party process cannot beacon the one IE that defines a Miracast sink. Miracast is Linux-only here, and `main.rs`'s refusal on non-Linux is the answer rather than a placeholder (#17). |
| `proto-matter` | **The whole Matter Casting control plane, commissioned end to end against a peer; never met a real phone.** The one protocol where the panel is the *commissioner* — the phone is commissioned onto a fabric this box administers before it can say anything, so the receiver is a certificate authority as well as a node (D54). Ours: User Directed Commissioning, which `rs-matter` does not implement at all, written byte for byte from `connectedhomeip`'s own encoder and fixture-tested against a hand-written vector; the commissioner-generated passcode flow, which is the only one that fits a device whose input is a person looking at it; the fabric and its root key; the endpoint tree (Casting Video Player plus a Content App per thing the panel can open); the cluster handlers; the tokio sockets; and the `_matterd._udp` record on the shared responder. `rs-matter`'s, below that: TLV, MRP, PASE and CASE both ways round, the interaction model, the certificate format, and every media cluster generated from the CSA 1.5.1 IDL. A scripted Casting Client drives the UDC exchange over real sockets and found the bug that mattered — five retransmits producing five different passcodes, changing the number while somebody read it. **Commissioning now runs against a peer end to end (#171, closed):** `checks.matter-vm` drives UDC → passcode on the glass → a scripted human reading it → `_matterc._udp` browse → PASE → `AddNOC` → CASE → one `ContentLauncher::LaunchURL` landing as a `session: play`. **Since then:** `matter-vm` hosts *two* content apps and a client drives every cluster — MediaPlayback, TargetNavigator, ApplicationBasic, `LaunchContent`, descriptor and ACL reads (#196); a failed commissioning sends a real `CommissionerDeclaration`, with the three producible `CdError` codes produced and the other six documented as having no producer (#198); the passcode comes off the glass when it expires, asserted over the real 180 s (#197); `node.rs` has its first unit tests; and a commissioned phone survives the death of its CASE session — the `_matter._tcp` operational record (`<compressed-fabric-id>-<node-id>`, with the `SII`/`SAI`/`SAT` keys rs-matter seeds MRP from) is published on the shared responder once the fabric has a member, and `matter-vm` restarts `castaway.service` and has a `--cast-again` peer with a persisted fabric resolve it and cast with no re-commissioning (#173); the seek surface reads the *pipeline* — `Duration` and the seek range refreshed from the playback report at every read and invoke, skips resolved in the handler into the absolute seek the pipeline is handed, a seek into media with no known end refused `SeekOutOfRange`, and `matter-vm` drives all three verbs plus both refusal statuses and reads the Descriptor's own attribute and command lists (#283); and the player endpoint grew `KeypadInput` (transport keys onto the shared drive path; `UnsupportedKey`/`InvalidKeyInCurrentState` both produced) and `ApplicationLauncher` (the catalogue as an ApplicationPlatform — `CatalogList`, `LaunchApp`-as-selection, `StopApp`/`HideApp`, `CurrentApp`), both exercised in `matter-vm` (#274). **Not done:** it has never met a real phone; commissioning is strictly serial with one global prompt slot (#209); the endpoint is not conformant and no certified sender will talk to a test-vendor identity (#172); and the honest content-app catalogue is the panel itself, because Matter carries no media and an app that accepted a cast it cannot play would be lying. |
| `moonlight-sys` | **Bindings, pinned and checked in.** FFI to moonlight-common-c, the linked GameStream core (D37). Regenerated from the same revision Nix builds; struct layouts guarded by the `moonlight-bindings` check (#191) — bindgen's own size/offset asserts turned out to be self-referential, so the check regenerates and diffs instead. |
| `proto-gamestream` | **Paired against real Sunshine; never yet streamed.** The one *inverted* protocol — the panel is the Moonlight client, so it browses and dials rather than advertising and waiting. Ours: mDNS host discovery, the NVHTTP API as request builders + rich response types, the gen-7 pairing handshake as a typestate machine, the client identity, per-host pairing persistence, and the adapter. Verified three ways: against Sunshine's own checked-in vectors (its `clientchallenge` ciphertext, its phase-4 hash, its `clientpairingsecret` signature), through all four phases over real sockets against a scripted host, and — the one that counts — against the **real `sunshine` binary in a VM**, which pairs, trusts us over mutual TLS, and serves its app list (`checks.gamestream-vm`). Linked, behind the `stream` feature (default-on since D55): RTSP, ENet control, FEC'd RTP video, encrypted Opus audio, input. **Not done:** no session has ever run against a real Sunshine host — everything between "the host said 200 to /launch" and pixels is unverified. The chooser exists now (D38's shell), and pairing is walk-up too: pressing an unpaired host puts a panel-generated PIN on the glass, holds the handshake open while someone types it into Sunshine's web UI, and refreshes into the app list — the config-driven startup pairing shares the same `pair()` and remains for headless boxes. The panel-initiated *screens* have never faced a real Sunshine (the underlying handshake has, via `checks.gamestream-vm`). The linked half is GPL-3.0 against this MIT tree; as of D55 it is on by default and the default artifact is therefore GPL-bound, which was accepted deliberately — an untested streaming path was judged the larger risk on a tree that is private and redistributes nothing. |
| `pipeline` | **Render path real.** Null backend (default) + wgpu compositor + ffmpeg decoder + RenderPipeline + winit kiosk behind `render`/`ffmpeg`/`kiosk` features. Browser: the Electron subprocess host behind `electron` (D36). What is on the glass is one model — `panel` (screens, surfaces, focus; everything else derived) — and how it *moves* between those states is `motion` (springs, one choreography table), both pure and unit-tested with no GPU (D46). The compositor grew an animatable corner radius and an independent source rect so a container of the wrong shape crops rather than stretches. The composited output is also *readable*: `/screenshot.png` is a one-shot CPU readback, and `/stream/*` is a live HLS duplicate of the glass (#101, D49) — RGBA→NV12 in a wgpu pass at the stream's own size, whichever H.264 encoder the box turns out to have, and fMP4/HLS boxing that is ours and byte-tested. Nothing is encoded until something fetches the playlist, and the tap retires ten seconds after the last request: measured on the dev box, zero CPU with nobody watching and ~5.4% of one core streaming 1080p30 off a 4K panel. The duplicate carries the panel's **sound** as well (D50, D52): tapped on the panel's one **audio mixer** — every source, and the browser's page audio, writes into a `MixInput` and the mixer owns the single device (#111) — and muxed as a second track in the same segments, both measured against one wall-clock origin so they cannot drift. The tap is fed the samples the device was given rather than a reconstruction of them, and the mixer is also what paces every source: a `Pull` source's `MixInput::write` blocks while it is already `OUTPUT_LEAD` ahead of the speakers, in flight across the ring *and* the device queue, while a `Live` source sheds its oldest frames instead — parking a live sender just moved the loss upstream into protocol queues, measured at 21–22% before #174. A device that refuses or vanishes — the panel sleeping and taking the HDMI sink with it (#55) — is the mixer's problem to retry behind sources that keep draining, not a session-ending error. The same encoded pictures are also fanned out live, in Annex-B with the parameter sets in band, to **WebRTC peers** (#18): `/remote/` is the panel made *touchable* from a phone, over one PeerConnection carrying the duplicate out and that peer's contacts back. Since the row was written: the visualizer taps the same mixer (16 Goertzel bands, D56); the video decoder is fed on dts so B-frames no longer arrive in bursts (#234); the scrubber interpolates between clock readings and follows a finger mid-drag (#165, #97); `[audio] record` writes the post-mix, post-gain stream to a WAV (#186); and `mixer-vm` measures the mixer against a clock that is not its own (#204). The remote track carries the same mix as **Opus** beside the pictures (#259) — encoded only while a peer is listening, negotiated under the offerer's payload type, and read back out of a real Chromium's decoder as the test tone's own waveform (`remote_browser.rs`). **Not done:** the upload to the hardware encoder is a readback, not a zero-copy handle export (#101); audio is stereo only, and leads the panel's own speakers by the output queue's depth. |
| `control-display` | **Trait and encoder only — no backend reaches hardware.** `NullDisplay` logs, and it is what `app` constructs unconditionally. `dell.rs` builds the RS-232 command frame (header/id/category/opcode/len/data/XOR) with **placeholder opcodes**, and nothing sends it. The `serial` and `ddc` features are empty feature lists — no `serialport`, no `ddc-hi`, no module behind either (#21). |
| `input-touch` | `TouchSource` trait + null; evdev/winuser feature stubs. The kiosk now routes each press to the browser *or* to the panel's transport strip, whichever owns the point touched (D33). |
| `app` | **Runs.** One HTTP host (DLNA+Spotify+DIAL) + one SSDP + one mDNS + session mgr. TOML config. The network surface is a registry (D45): `surface.rs` generates `docs/network-surface.md` and `nix/network-surface.json` (freshness-tested), the NixOS firewall derives from the JSON, media planes bind from `[media_ports]`, raw binds are clippy-denied outside registered sites, and `--network-surface[=json\|netsh]` prints the resolved view. |

## The panel's own controls (`cargo run -p pipeline --features render --example card_preview <out.png> [cover.png]`)
A transport strip under the now-playing card: previous / play-pause / next, plus shuffle
and repeat when the source reports them, plus a scrubber with elapsed and total that
accepts a touch to seek. Spotify and Bluetooth share it, because they share the card —
which buttons appear is decided by the session's `ControlCapabilities` and nothing else
(D33), so a phone gets what AVRCP passthrough can express and a Spotify session gets the
lot. A source with no duration gets elapsed time and no bar rather than a scrubber drawn
against a guess.

The preview writes PNGs of the real renderer for a person to look at. The logic under it
— layout, hit test, and press → `ControlTxn` — is pure and tested without a rasterizer,
including the panel-normalized → strip-local mapping, whose failure mode is the quiet one
where the buttons still draw correctly and answer to a different part of the glass.

## DLNA conformance
`docs/dlna-conformance.md` records a spec-grounded adversarial review of `proto-dlna` and
the media path it feeds — held against AVTransport/RenderingControl/ConnectionManager, UDA
1.1, the UPnP AV schemas, and the Rygel and gmrender-resurrect sources rather than against
memory. The defects it found were GAPS.md G68–G82, all since closed (see #104); this file holds the rest, which is the
larger half: what is correct and must not be "fixed", how real control points diverge from
the text, and what the citations are worth (DLNA Guidelines Part 1 is paywalled, so every
`7.x` section number in these docs is secondhand).

## Verified against the other implementation (`nix flake check`, no hardware)
Two checks compile openscreen — the reference implementation of Cast — and put our code
and theirs on opposite sides of the wire. Neither needs a sender, a network, or a person.

- `openscreen-rtp-fixtures`: openscreen's `RtpPacketizer` + `FrameCrypto` generate the
  golden mirroring stream our receiver is tested against (#53/#54).
- `openscreen-device-auth`: openscreen's *sender-side* verifier judges device-auth
  responses we produced (D31). Eight vectors. The one that matters says we fail against
  the roots senders ship, and fail only for the root. That was long read as "a provisioned
  credential (#40) is the whole remaining distance to an official sender" — and #226
  disproved it: the distance was DNS-SD sub-types, and the same borrowed identity is the
  one a real Play Services picker now lists. #40 stays open as "an identity of our own",
  not as a blocker.

## Verified against the reference implementation itself (`gamestream-vm`)
The GameStream check is the only one here where the *reference implementation is the
peer*, not a script of ours. `nix build .#checks.x86_64-linux.gamestream-vm` boots plain
nixpkgs `sunshine` in one VM and our `gs-probe` in another, and pairs them over a real
LAN with no hardware and no person — `sunshine -0` takes the PIN on stdin, which is what
replaces someone typing it into its web UI.

It exists because the unit tests cannot fail in the one way that matters. Those check our
pairing crypto against Sunshine's own checked-in vectors, which proves we agree with the
vectors; and `tests/pairing_over_http.rs` drives a scripted host, which is *our reading*
of Sunshine's source on both sides of the wire. Only the real program can say the reading
was right.

What it asserts, each failing differently: the four-phase handshake completes and the
certificate persists; the host itself, over mutual TLS, reports us as paired; an
HTTPS-only endpoint answers with a non-empty app list; the host self-identifies as
Sunshine (every GFE-only workaround hangs off that bit); the `/launch` query is
well-formed enough to be *judged* rather than rejected as malformed; and the pairing
survives a restart with no PIN, which is what catches a client that regenerates its
identity per boot.

**Verified 2026-07-28**, and worth recording what the real program taught us that our
reading of its source did not. Sunshine's `-0` flag (PIN on stdin) is a trap: it closes
stdin once up, so writing the PIN into a FIFO blocks forever and presents as a hang with
nothing in any log — the PIN goes over its web API instead. Its main loop *is* the
system-tray loop, so a headless host answers NVHTTP for about a second and then exits;
`system_tray = disabled` is what keeps it alive. And its `/api/pin` handler acts on
whichever pairing session is in flight, so submitting before the client asks is a silent
no-op — the test retries until it lands.

`nix run .#gs-probe -- <host> --pin 1234` is the same binary pointed at a real host by
hand — the fastest way to find out why a panel will not pair.

## Verified working end-to-end (tier-2 VM test, no human in the loop)
`nix build .#checks.x86_64-linux.integration-vm` boots the receiver from the real NixOS
module in one VM and drives it from a *second* VM over a real LAN with scripted senders —
so the deploy path and the tested path are the same path, and discovery is real rather
than hidden by loopback. Each of these asserts the sender's view **and** greps the
receiver's journal, so the event is proven to cross adapter → session manager → pipeline:

- **SSDP**: M-SEARCH answered; every advertised `LOCATION` is fetched from the other host.
- **DIAL**: that a *browser-less* build advertises nothing, mounts nothing, and says why.
  Launch, DELETE/dismiss, relaunch inside the grace window, and the two-senders-on-one-
  screen attach path are all driven with no internet and no human (#96); the real YouTube
  path is `yt-selfplay` below.
- **DLNA**: `SetAVTransportURI`/`Play`/`Pause`/`Stop` walk
  `NO_MEDIA_PRESENT → PLAYING → PAUSED_PLAYBACK → STOPPED`. And the first non-us peer
  DLNA has ever had: a *third-party* control point (`async-upnp-client`'s `DmrDevice`,
  what Home Assistant's `dlna_dmr` drives) parses the description and all three SCPDs,
  subscribes to all three services, drives the transport, and round-trips DIDL through a
  foreign parser (#202).
- **Cast**: a hand-rolled CASTv2 sender does TLS → CONNECT → PING → GET_STATUS →
  GET_APP_AVAILABILITY → a refused LAUNCH → LAUNCH `CC1AD845` → LOAD → PAUSE → CLOSE. The
  availability reply and the `LAUNCH_ERROR` are asserted from the sender's side *and* in
  the journal, because the failure they prevent — claiming an app we cannot host — looks
  like success on the wire; the LOAD reaches the pipeline and the CLOSE ends the session.
- **AirPlay**: pipelined `OPTIONS` + `GET /info` in one write (bare-path URIs), the `/info`
  binary plist parses, `POST /pair-setup` answers 200 with the 32 raw Ed25519 bytes that
  equal the advertised `pk`, HomeKit pairing is refused `501` rather than faked, `TEARDOWN`
  ends the session. Plus what the advertisement does *not* say: no FairPlay in `et`, no
  codec in `cn` we do not decode, no empty `pk`. Those assertions exist because the failure
  they catch is silent — a bit we cannot honour makes a real iPhone appear to find us and
  then do nothing at all.
  **This is where the AirPlay VM coverage stops**: no `ANNOUNCE`, no `SETUP`, no `RECORD`,
  no `/fp-setup`, no RTP. AirPlay is the only advertised protocol whose media plane has
  never crossed a real LAN in CI — audio and mirroring are driven over real sockets, but
  only in-process (`tests/raop_harness/mod.rs`, with the decode seam in `crates/app`
  proving real ALAC on both the clear and the encrypted legs — #189).
- **mDNS**: `_spotify-connect._tcp`, `_googlecast._tcp`, `_airplay._tcp`, and `_raop._tcp`
  are all browsable from the sender with the ports that actually answered. The responder's
  record set has since grown `_matterd._udp`/`_matterc._udp`, `_display._tcp` (MICE), and
  the Cast DNS-SD sub-types (#226).

## A Spotify session, with no phone (`cargo run -p proto-spotify --example selfplay -- http://<receiver>:8080`)
The Spotify sibling of `yt-selfplay`, and needed for the same reason: Spotify's cloud is a
third party to every part of a Connect session, so a test that stops at "the receiver
answered `status: 101`" tests almost none of it. Pairing succeeding says nothing about the
login behind it — that is the failure this exists to name.

It logs in as the harness account, converts that into the reusable credentials a phone
would hold, wraps them the way a phone wraps them, POSTs `addUser`, then waits for the
device to appear in the *account's* device list before transferring playback, queueing a
second track, checking it really reached the queue, skipping, and checking the queued
track is now playing. Tracks are chosen at runtime with `market=from_token`, so a
licensing gap in one market does not look like a receiver bug.

Credentials come from `.env.local` (gitignored; see `.env.example`). One browser visit is
needed the first time — Spotify has no device-code flow — after which the cached refresh
token makes every run hands-free. Neither the harness nor the receiver needs an Android VM
or a headless browser.

**Not yet run end to end**: it needs the Premium account authorised in a browser once,
which has not happened. The offline half of the same path *is* covered — `tests/pairing.rs`
drives `getInfo` → DH → blob → `addUser` against the real router in-process.

Automating that one browser step was tried and abandoned, so nobody has to try it again:
a real (non-headless) Chromium under Xvfb, driven by selenium, reaches
`challenge.spotify.com/.../recaptcha` — "we need to make sure that you're a human", a
reCAPTCHA Enterprise anchor plus image `bframe` — immediately after the username step.
Getting past that means defeating an anti-bot control rather than automating a UI, which
is out of scope on purpose. The step stays human, stays once, and the harness now writes
the resulting refresh token into `.env.local` itself rather than asking for a paste.

## Which package to run
- `packages.default` — on Linux, the full kiosk, so `nix run .` is a receiver you can
  actually cast to: render pipeline + Electron browser + audio out + Bluetooth, wrapped
  with `CASTAWAY_ELECTRON`/`CASTAWAY_BROWSER_APP`/`LD_LIBRARY_PATH` so it runs outside the
  devShell. **This is the one to deploy on Linux** (`services.castaway.package`). Every
  optional feature is on, `ldac` included as of 2026-07-29 — it links Sony's own
  `libldacBT` for the one A2DP codec ffmpeg cannot decode (#14, D47). LDAC is advertised
  by default as of 2026-08-09 (#253): it sorts *first*, so it is what every capable phone
  negotiates, which waited until a real Android phone had streamed to it (2026-08-08
  bench session; the capture is checked in and decodes under a cross-correlation pin).
  The runtime off-switch is the config's codec list:

  ```toml
  [bluetooth]
  codecs = ["sbc", "aac", "aptx", "aptx-hd"]
  ```

  Verified 2026-07-26 in its pre-D36 `--features cef` form: built from the flake, run
  headless on Xvfb, and passed both `yt-selfplay` modes with real video composited at 4K.
- `packages.castaway-portable` — no renderer, no browser, nothing platform-specific.
  Serves and discovers; **cannot** play YouTube, and honestly declines to advertise DIAL
  (D27). Since D55 this is **not a product**: it builds with `--no-default-features` and
  survives because the VM tests boot it — they assert on the null pipeline's log lines,
  which is how a protocol test proves an event reached the media plane without a GPU in a
  VM. Since D57 (#207) the flake targets only `x86_64-linux` — Darwin was unbuildable and
  aarch64 equally aspirational — so the portable build survives purely as that fixture.
- `packages.castaway-windows-electron` — the Windows deploy artifact, cross-compiled
  (append `.archive` for the same tree as a single zip).

## A YouTube cast, with no phone (`nix run .#yt-selfplay -- http://<receiver>:8080`)
The one path a VM test cannot cover: YouTube's Lounge servers are a third party to the
session, so this needs the real internet and a running `--features electron` receiver. It is a
scripted phone — DIAL launch with a `pairingCode` it invented, wait for the receiver's
page to register that code with YouTube, bind to the Lounge session as a remote control,
queue videos, and assert the screen actually plays them. **Verified 2026-07-26** against
the (then-CEF) kiosk on Xvfb: three taps, each confirmed playing, plus 4K screenshots of real
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

## SponsorBlock (`[sponsorblock]` in castaway.toml, needs the `electron` build)
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

`skip_ads = true` also presses the screen's own Skip Ad button once `isSkipEnabled` flips
— **implemented and unit-tested against a captured ad payload, but the live press has not
been observed**: YouTube served an *unskippable* 15s pre-roll during capture and no
skippable one since. The command encoding is verified accepted (200). Unskippable ads
still play, and nothing is muted — a mute that failed to lift leaves a silent display.

Lookups use the hash-prefix endpoint: the server sees four hex characters of
`sha256(videoId)` and never the video. The database is CC BY-NC-SA — non-commercial use
fits, attribution rides on the toast, and segments are deliberately never written to disk
(that would be redistribution).

## Render path — actual pixel output (GPU-verified)
All in the default feature set since D55, and the suite runs under `nix flake check` on
lavapipe (#98) — the native devShell is only needed for the hardware-specific tests.
Eight golden scenes gate the composited output, two tolerances each (#203).
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
- **Hardware decode** (`hwaccel` feature, #58): VA-API → DMA-BUF → Vulkan import → NV12
  sampled in the shader, with **no copy anywhere**. `tests/hwaccel_zero_copy.rs` decodes a
  known colour on the dev box's RX 7900 XTX and asserts on the composited pixels, which is
  what catches a wrong tiling, wrong plane pitches, or the wrong colour matrix — all of
  which render a picture rather than an error. The hw/sw choice is runtime, not a build
  flag, and falls back to software mid-session with a log line. The Windows half
  (D3D11VA → shared NV12 texture → D3D12) **ran on the Dell on 2026-07-31** and paid for
  the trip three times over — a `mem::forget`-ed browser ack, a per-frame D3D12 resource
  leak, and an import cache keyed on a per-frame handle value (#58; consumer→producer
  sync and 10-bit remain).
- **Kiosk**: winit borderless-fullscreen surface path. Now run on the dev box's live
  display rather than only compile-checked — see #59, where the idle loop was measured at
  0.00% of a core after the move to demand-driven rendering. The *panel* is still the
  unknown: nothing here has met the C6522QT.
- Run it: `cargo run -p castaway` — render/ffmpeg/kiosk are default features since D55
  (opens a fullscreen window; cast a video via DLNA to see it decode+display).

## The panel's shell (D38, `docs/app-shell.md`)
The idle screen became a launcher. A tile per enabled service; tapping one opens that
service's own screen with its instructions and the exact name to look for, and tapping
Moonlight opens a list of hosts, then that host's apps, then streams one. Every screen
has a way back, and the way home is a left-edge swipe or a pill that appears on any touch
and fades — the pill for someone who has never used the panel, the swipe for when it is in
the way.

Bringing the shell forward demotes whatever is playing to a corner rather than stopping
it, and tapping that corner hands the panel back. A session ending returns Home, unless
the panel was touched in the last twenty seconds.

`cargo run -p pipeline --features render --example shell_preview -- <dir>` dumps every
screen to PNG, which is how the layout was reviewed — hit-testing is unit-tested, but
whether a screen reads from across a room is not something a test can answer.

**Since done:** screens animate between each other (`render_pipeline::Transition`, on
the lossless command lane because a dropped transition desynchronises the panel from its
own state machine), a picker longer than the panel scrolls (`picker.scroll`, fractional so
a drag moves smoothly), and #24's theming is under way rather than unstarted —
`pipeline::theme` is one palette for every surface, taken from dma.space's own CSS with
provenance in `assets/brand/README.md`, and the mark and mascot are drawn. The
easter-egg palettes are in (#263): `pipeline::theme::season` maps the local date to a
seasonal background — Pride month, the awareness days and weeks, the twelve days of
Christmas, Halloween — mixed into the panel's own dark ramp at 22% so the room changes
and the text stays readable; `theme = "auto" | "plain" | <season>` in castaway.toml
follows, silences or forces it, and a midnight task rolls the Home screen over without a
restart. `attract_preview`/`shell_preview` take a palette name to render any of them to
PNG. **Still not done** on the theming side: DejaVu is still the primary face — though
`Fonts` is now a fallback chain with a bundled CJK subset, so a Japanese track title
renders instead of tofu (#88) — and the scrolling long titles and blurred pillarbox
borders #24 asks for are unwritten.

## Driving the panel from a phone (`/remote/`, #18)

The sibling of `/stream/*` and a different want: not "show me the panel" but "let me use
it". Everything below is code-complete and tested without hardware; **none of it has been
driven from a real phone against a real panel**, which is the gap that matters.

**Why WebRTC and not the HLS the same encoder already feeds.** Latency, and then the
deployment. HLS is one-second segments with a window of eight — three to six seconds
glass-to-glass, which is fine for watching and unusable for control, where you cannot tell
which tap did what. And the far end is a phone on Wi-Fi: a fixed-bitrate stream over TCP
turns a lossy link into an unbounded stall whose only recovery is falling behind and then
seeking to the live edge. UDP with a jitter buffer degrades instead. One encode feeds both
— HLS gets AVCC boxed into segments, a peer gets Annex-B with the parameter sets in band,
because there is no init segment in RTP.

**The input rides the same connection.** A data channel defaults to reliable and ordered,
which is what input needs — a lost `Up` after a `Down` strands a contact for the session.
Given that, one PeerConnection is one *lifecycle*: "the peer went away" is a single event
with a single handler, and that is where the nastiest bug in the feature lives.

**A contact's identity carries its origin.** The router used to key its maps on a bare
`u32`, with the mouse's stand-in reserving `u32::MAX` behind a comment. Two phones both
numbering their first finger `0` would have merged into one drag. `ContactId { origin, raw }`
makes that unrepresentable, `RemoteId` is never reused so a reconnect cannot inherit what
the last connection left behind, and `cancel_origin` lets one peer drop without yanking
anybody else's gesture. A dropped peer's contacts are **cancelled, not released**:
completing the gesture would commit whatever it was over, which on the transport strip
means seeking to wherever the finger was when Wi-Fi died.

**A remote contact takes the same road as a finger**, so the edge swipe, the home pill, the
transport strip and the shell all work on it for free — that is what the decode/apply split
in `kiosk` bought. The one thing it cannot do is the *gesture* home: a left-edge swipe is
the Android back gesture and iOS swipe-to-go-back, and the browser eats it before the page
sees it. So the page has a Home button, and it travels through the same queue as the
contacts so it keeps its place against them.

**ICE ports are pinned and declared** (`[remote.ice_ports]`, 41032–41063). Not tidiness:
`surface.rs` generates `nix/network-surface.json` and the NixOS module derives the firewall
from it, so a candidate outside a declared range is one the deployed box silently drops —
the connection would negotiate and then carry nothing.

**Security is a real change of kind.** Port 8080 has no authentication, so this turns
"anyone on the LAN can watch the panel" into "anyone on the LAN can drive it". That is
stated in `docs/network-surface.md`'s Security column rather than left implicit, and
`remote.input = false` keeps the viewing half while dropping every input message at the
boundary. It is not a control against someone who can already reach the port.

**One player, on `/`, stopped until pressed.** It is not a page of its own: a viewer at
`/` and a driver elsewhere is one place too many to look at the panel. Nothing is
negotiated or encoded until the button is pressed, and pressing it is also what turns the
picture from a video into an input surface — there is nothing to pause or scrub about the
panel as it is right now. The two states are exclusive by construction, not by ordering:
the capture layer is `display:none` until `ontrack` fires.

**Tested without hardware:** the wire parse (a peer cannot name another peer's contact; a
coordinate that is not finite is refused before anything clamps it), the coalescing queue
(a run of moves collapses; an `Up` is never dropped under flood; a departure keeps its
place in the order), the router (three contacts all numbered zero stay three), and a real
negotiation over real sockets.

**Tested with a real browser** (`remote_browser.rs`, `#[ignore]`, needs a GPU and an
Electron — which is Chromium): the real player loads stopped, connects when pressed, and
the *browser itself* reports frames out of its decoder; then a touch goes in through the
browser and comes back out of the panel's input queue as a contact belonging to that peer.
This is the test that matters, and writing it found three bugs the fixture-based one
structurally could not — a fixture agrees with whatever you send it.

**The keyboard (#260)** rides the same queue. The design constraint is the phone's IME:
autocorrect, swipe typing and CJK *compose*, so the page forwards no key stream — the
tray's field travels as its value's diff (`text` messages, `Input.insertText` on the far
side), and only the strokes text cannot say — Enter, arrows, backspace on an empty field
— travel as `key` messages (`Input.dispatchKeyEvent`). Both keep their place in the queue
against the contacts around them, for the same reason Home does: Enter after the tap that
focused a field must land after that tap. `remote.input = false` drops these at the same
boundary as everything else. Tested at both halves with a real browser: typing in the
real tray comes back out of the panel's queue as text-then-Enter (`remote_browser.rs`),
and `InputSink::text`/`key` land in a real page's field through CDP
(`browser_end_to_end.rs::typing_reaches_the_page`).

**The remote has sound (#259).** The same mix the HLS duplicate's AAC track carries —
tapped at the mixer, so it is the samples the speakers were given — is encoded a second
time as Opus (`stream::opus`, libopus, 20 ms frames) and fanned out beside the pictures
(`LiveFeed::publish_audio`), because WebRTC does not take AAC. Each peer gets an audio
track with its own SSRC, stamped with the *offerer's* payload type like the video, and a
peer whose offer carries no audio m-line degrades to pictures-only rather than being
refused. Nothing is Opus-encoded while no peer is listening. The player offers the audio
transceiver and unmutes only when the track arrives, under the play press's sticky
activation. Proven against a real browser: the negotiation under the offerer's numbering
(`remote_negotiation.rs`), and the decoded waveform read back out of Chromium at the
tone's own amplitude (`remote_browser.rs`). The pinned BtbN Windows ffmpeg carries
`--enable-libopus` (checked in its `avcodec-61.dll` configuration string), so the path
is not Linux-only.

**Not done:** no *phone* has driven a real panel, though a real browser now has; and the
remote's sound trails its pictures by roughly `audio_settle` (150 ms), because the Opus
encode draws from the same settled mix the AAC track does.

## Biggest open items (see the issue tracker)

Rewritten 2026-08-08 against the issue tracker. The struck-through history this list used
to carry now lives in the closed issues — including, since the last rewrite, #52 (the
2 fps LOAD was a demux/clock deadlock), #74/#75/#76 (the phone visit happened, and the
iPhone capture is checked in), #80 (the AirPlay video path is built), #81, #83, and #171.

**Blocked on the panel, or on hardware we do not have.** Nothing here is a code question:

1. **#58** — the Windows D3D11VA → shared-NV12 → D3D12 decode bridge **ran on the Dell on
   2026-07-31** and found three real bugs, all fixed. What remains is consumer→producer
   sync and 10-bit.
2. **#64** — the same shape one layer up: Electron's shared-texture OSR now runs and
   paints on Windows too, but the keyed-mutex/fence half is unproven. The fd transport
   is no longer part of this: production is `SCM_RIGHTS` over a socket beside the
   control one (#271) — sent by the one native piece in the host app
   (`castaway-browser-fd`, a hand-rolled N-API cdylib, because Node cannot say
   `sendmsg(2)`), received by `electron_fd_plane`, asserted zero-`pidfd_getfd` in
   `browser_end_to_end` — with the pidfd reach-in kept as the logged fallback for a
   host app without the addon.
3. **#190** — the GameStream media plane has never run against a host with a real
   encoder; everything up to `/launch` is proven against real Sunshine. And **#167** —
   a streamed session cannot be *touched*: `moonlight-sys` binds every `LiSend*` entry
   point and `stream.rs` calls none of them, on a touch panel. (Neither is blocked on
   hardware — a software-encoding Sunshine in the existing VM reaches both. See
   `docs/test-matrix.md` §4.8.)
4. **#17** — no real Miracast driver. hwsim is the best-behaved mac80211 there is; the
   interface-combination parse and the 5 GHz NO-IR question both need a radio. **#206**
   is what that means for the deploy target.
5. **#65** — touch through CDP has never met glass. **#18** rides the same path from the
   other end and is further along: a real browser drives the real player end to end in
   `remote_browser.rs`, frames decoded and touches round-tripped. What is untested there
   is a *phone*, and the panel's own glass.
6. **#21** — there is still no display-control backend at all: `serial` and `ddc` are
   empty feature lists, `dell.rs` is a frame encoder with placeholder opcodes, and `app`
   constructs `NullDisplay` unconditionally. **#55** — what that costs — is largely paid
   down in the mixer (a sink that stops taking audio no longer deadlocks a session, and a
   device that comes back is reopened and heard), but the HDMI-sleep case itself still
   has no test snd-dummy can reproduce.

**Blocked on a capture or a credential.** Ground rule 9's cost, itemised:

7. **#40** — a Cast identity of our own. No longer a discovery blocker: the CRL was
   measured 2026-08-08 and revokes neither borrowed identity, and the borrowed CKS
   identity is the one a real Play Services picker lists (#226). What is left is wanting
   a credential that is ours to keep rather than someone else's to revoke.
8. **#48** — the Spotify pairing blob is round-trip tested against our own encoder and
   nothing else. A wrong split fails as "pairing expired", indistinguishable from a stale
   blob. (#200 pinned `getInfo`'s twenty fields — a pin, not a validation.)
9. **#41** — no golden YouTube Lounge bind-channel transcript.

**Open on measurement.** These need a session and a log line, not hardware:

10. **#79 / #176** — the same defect from opposite ends, and #176's wire half now runs:
    the plist `SETUP` finally yields a timing peer (it used to come only from the RTSP
    `Transport` header), T1 watches the type-82 probe leave and the type-83 reply fold
    into `clock_samples` in both session shapes, the resend request has been observed
    leaving a socket, and the declared latency is parsed wherever a sender states it.
    What remains: *consuming* that declared lead as a target buffer depth instead of
    front-padding's accident (#176), and #79's skew bound, which still wants a
    real-device capture (#150 stopped the false report).
11. **#177 / #278** — the pacing half is resolved (page audio measured at 100.0% real
    time, `shed=0`, on the b540764 build), and the skew figure now means something: the
    browser's two timestamps share no origin, so `av_skew_ms` is drift on the session's
    first pairing (`pipeline::av_skew`, #278) rather than the origin difference the old
    subtraction reported. Measured live: |skew| ≤ 100 ms held over a session
    (`browser_end_to_end.rs::av_skew_is_drift_on_one_baseline_not_the_origin_difference`).
12. **#87** — AVRCP cover art renders for some tracks and not others over Bluetooth.

**Open on a decision.** #82 (should an AirPlay session be controllable from the panel at
all, given the phone is in the room — and DACP credentials arrive in every request), #72
(party mix, deferred and wanted), #168 (a panel volume control, and the push-back that
becomes worth writing once one exists). #16's shape is no longer the question — the
platform shim is built and checked — but its real-sender leg is: **#228**, no real sender
has ever launched an app on this receiver, and **#226**'s residual, no completed mirroring
session from the phone the picker now lists.

**The test audit's long tail** (`docs/test-matrix.md`) is its own family: #184 (no
automated test uses a real Cast sender), #185 (Cast mirroring audio/pixels), #188/#189
(AirPlay's media plane has never crossed a real LAN in CI), #205 (the doc's performance
numbers are one-off measurements, not gates), #234 (the AV suite asserts structure where
a human perceives timing).

## Browser + adblock + YouTube (proven on CEF; the runtime is now Electron — D36)

> **Superseded by D36:** the in-process CEF host this section describes was replaced by the
> Electron (castLabs ECS) subprocess — `electron` feature, `pipeline::electron_browser` —
> and the CEF crates, packages, and `cef` feature are gone from the tree. The section is
> kept as the record of what was verified and how; the adblock/scriptlet/YouTube machinery
> it describes carried over to the Electron host. One property was lost in the port and
> restored by #239: the daily refresh reaches a *running* receiver again — under Electron
> the browser's query path reads the shared cell at every decision, so the render-side
> rebuild (and the cache-stamp check that triggered it) has no successor and does not need
> one. Verified live in `browser_end_to_end.rs`: an engine installed mid-session blocks
> the next load's requests with no respawn.

The doc's "boss fight" is won — CEF builds, links, and **runs** reproducibly against nixpkgs
`cef-binary` (flake `cefDist` + `CEF_PATH`; no download/patchelf). Proven with screenshots:
- `pipeline::cef_browser` — offscreen Chromium via cef-rs; renders real pages headlessly
  (SwiftShader) → `on_paint` BGRA → `CefFrameSink`. Subprocess entry point, TV user-agent.
- `pipeline::cef_adblock` — Brave adblock-rust in CEF's `ResourceRequestHandler`, cancels +
  **logs** blocked requests (`castaway::adblock`), **and** computes per-page scriptlet
  injections. `filterlists` subscribes to EasyList *and* uBlock Origin's list (fetch → cache
  → built-in fallback; `CASTAWAY_FILTERLISTS_OFFLINE=1` pins to cache).
- **Scriptlet injection is live** — a render-process `on_context_created` handler runs uBO's
  `##+js(...)` code inside the page *before its own scripts*, which is the only timing at
  which hooking `fetch`/XHR works. Verified by planting a probe rule and watching it come
  back through the page console (`castaway-injection-ok src=castaway://scriptlets`). Rules
  auto-update, **and so do the scriptlet bodies**: uBO's module graph is evaluated in QuickJS
  and its own `builtinScriptlets` registry read back, giving 148 resources (the pinned legacy
  bundle gave 37) and a 38 KB injection on youtube.com — see #60/#62.
- **Lists refresh daily**, not just at boot, and a *running* receiver picks it up: the engine
  is swapped behind a shared cell, and render processes rebuild when the cache timestamps
  move. Verified by editing the cache under a live receiver and watching a probe rule start
  injecting with no restart.
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

## GameStream (`[gamestream]` in castaway.toml; the streaming core is on by default since D55)
The panel as a Moonlight client. On by default, but inert until a host is paired: pairing
is a PIN exchange someone must confirm on the *host's* side, so until then the adapter
only browses mDNS. Pairing starts from the panel itself — pressing an unpaired host in
the Moonlight picker shows a generated PIN and waits (three minutes, then a retry screen)
for it to be typed into Sunshine's web UI; the `pair_host` config below is the headless
alternative, not the only way in. The PIN is never configured — it authenticates one
handshake, so the receiver generates one and logs it at `info` rather than keeping a
plaintext credential on disk that is also re-sent on every restart (#78). The Linux kiosk build links the streaming
core. Since D55 it is on by default (D37's licence gate was dropped deliberately — see D55).

```toml
[gamestream]
# the credential — persistent, mode 0600. Omit it: the default is the platform state
# directory + /gamestream, which under the NixOS unit is this same path.
state_dir = "/var/lib/castaway/gamestream"
pair_host = "10.0.0.7"                        # PIN is generated and logged; safe to leave set
autostart_host = "10.0.0.7"                   # optional; unset means nothing starts
autostart_app  = "Desktop"                    # optional; unset takes the host's first app
width = 1920
height = 1080
fps = 60
bitrate_kbps = 20000
```

Pairing is a person walking to the PC: we hold a request open while they type the PIN into
Sunshine's UI. There is no `pair_pin` key any more (#78) — the PIN is generated per
handshake and logged at info, the resulting certificate is persisted per host, and an
already-paired host is detected and skipped, which is what makes `pair_host` genuinely
safe to leave set.

**The chooser exists (D38); the missing piece is input.** The panel lists hosts, pairs
walk-up with a PIN on the glass, picks an app, and streams it — and then cannot be
touched: `moonlight-sys` binds every `LiSend*` entry point and `stream.rs` calls none of
them (#167). On a touch panel, that is now the gap that keeps this protocol from feeling
like the rest of the receiver.

## Logs (`[log]` in castaway.toml, D39)
Two sinks, two filters. The console follows `RUST_LOG` (or `[log] level`) and belongs to
whoever is watching the box; the file follows `[log] file_level` and belongs to a panel
running unattended, so turning the console up to `debug` does **not** turn the file up with
it — the mirroring paths log per frame.

```toml
[log]
level      = "info"    # console; RUST_LOG wins over this
to_file    = true      # rotated files on disk
file_level = "info"    # deliberately not inherited from `level`
rotation   = "daily"   # minutely | hourly | daily | never
max_files  = 14        # pruned oldest-first at each rotation; ignored when `never`
# directory = "..."    # default: the platform log directory (below)
```

Under both sinks sits a **noise floor**: libraries that log per frame, per packet or per
poll are held down to a level where they still report trouble. It is not cosmetic —
`wgpu_core` logs `Device::maintain` at *`INFO`* once per presented frame, so without it the
on-disk log fills at the default settings; and `mdns_sd` at `debug` was 2168 of 2179
console lines in the first five seconds with only DLNA enabled. The floor is composed
*under* whatever you ask for, so `RUST_LOG=debug` means castaway at debug; naming a target
explicitly gets it back (`RUST_LOG=info,wgpu_core=debug`).

Files land as `castaway.2026-07-28.log`, dated so a restart appends to today's rather than
truncating it. Writes are synchronous: the release profile is `panic = "abort"`, and an
aborting process never flushes a background writer's buffer — which is exactly the tail
worth having.

## Where files live (`castaway-paths`, D39)
| | Linux (XDG) | Windows |
|---|---|---|
| state | `$XDG_STATE_HOME/castaway` | `%LOCALAPPDATA%\castaway\state` |
| cache | `$XDG_CACHE_HOME/castaway` | `%LOCALAPPDATA%\castaway\cache` |
| logs | `$XDG_STATE_HOME/castaway/logs` | `%LOCALAPPDATA%\castaway\logs` |
| config | `$XDG_CONFIG_HOME/castaway` | `%LOCALAPPDATA%\castaway\config` |

Under the NixOS module the unit sets `XDG_STATE_HOME=%S` and `XDG_CACHE_HOME=%C`, so those
resolve to `/var/lib/castaway` and `/var/cache/castaway`. Both exist because a dynamic
user's home is `/`: without them the paths become `/.local/state/castaway` and
`/.cache/castaway` under `ProtectSystem=strict`, unwritable, and every failure there is
swallowed by design (G31). If the environment names no home at all, the receiver logs a
warning naming the fallback rather than writing state somewhere it will not be found again.

## Design decisions worth your review
D7 (router composition vs SourceAdapter), D9 (hand-written prost, no protoc), D16 (socket
protocols advertise-gated), **D37 (GameStream links the Moonlight streaming core rather
than reimplementing it — a second carve-out from ground rule 9, made on instruction, and
one that puts GPL code behind an opt-in feature)**, **D54 (Matter links `rs-matter` for
the same reason and at a fraction of the price — the third carve-out, and the one where
the panel ends up running a certificate authority and keeping its own root key)**, and
**D30 (Spotify is the one protocol we do not reimplement
— a carve-out in ground rule 9, so worth disagreeing with early)**. D30 supersedes D10,
which deferred Spotify playback. Since this list was written: D49/D52 (the stream
duplicate and the one mixer — D50 is superseded by D52), D51 (the remote UI), D53 (the
panel lifecycle), **D55 (every feature on by default — the retro behind the header's
test-count claim)**, D56 (the visualizer, renumbered from a duplicate D54), and D57 (one
system: `x86_64-linux`). All in DECISION-LOG.md.
