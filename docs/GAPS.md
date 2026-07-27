# Implementation Gaps — Spotify, YouTube, Bluetooth

Audit of 2026-07-26, across `proto-spotify`, the YouTube path (`proto-dial` + `sponsorblock`
+ the CEF browser), and the Bluetooth stack (`hci-transport`, `substrate-hci`,
`substrate-l2cap`, `proto-bluetooth-audio`, `substrate-sdp`).

Companion to DECISION-LOG.md (why) and OPEN-QUESTIONS.md (what needs a human). The
difference: OPEN-QUESTIONS is blocked on captures, credentials, or a design call. Nothing
here is blocked on anything — these are things we can fix, that we have not.

Each entry: what is missing, the file it lives in, and **the consequence a person in the
room would actually observe**. That last part is the point. A gap that presents as an
error is a bug; a gap that presents as a receiver which looks completely healthy and does
nothing is the failure mode this project keeps rediscovering, and those are marked
**silent**.

`CONFIRMED` means the code was read and definitely does not handle the case. `SUSPECTED`
means the code path is right but the real-world trigger rate or peer behaviour is unproven.

## Status

✅ fixed · 🟡 partly fixed · ⛔ scoped out · unmarked = open. Fixed entries keep their
original wording, because the reason a thing was wrong is worth keeping after it stops
being wrong — several of these were made independently in two or three subsystems, and
the next protocol will be tempted to make them again.

**All three themes are closed.** Every subsystem is supervised (G1, G2), `Pipeline::control()`
reaches the speakers (G9, G10), and the display is handed back when another source takes
the session (G6) — with the outgoing source told to stop sending (G37) and able to take
it back by pressing play (G5).

Decisions taken, so nobody re-litigates them from the entry text alone:

- **G34 — scoped out.** Guests get put on a network that can see the receiver. Cross-VLAN
  TV-code pairing is not worth the keyboard/D-pad input path it would need.
- **G30 — partly.** No Broadcom/MediaTek/Qualcomm loaders: the deploy box is Realtek or
  Intel, and porting three firmware sequences for hardware nobody will plug in is not a
  good trade. The *silent* half is fixed — an unrecognised controller now says so instead
  of getting `NoInit` and producing an inert radio with nothing pointing at the cause.
- **G46 — Widevine is packaged on Linux only, which is not the deploy target.** nixpkgs'
  `widevine-cdm` ships `_platform_specific/linux_x64/libwidevinecdm.so` and declares
  `meta.platforms = [x86_64-linux aarch64-linux]`, so `packages.castaway-cef` plays
  DRM-gated content and `packages.castaway-windows-cef` — the artifact that actually
  ships — does not. Chrome for Windows carries `widevinecdm.dll` under
  `_platform_specific/win_x64/`, so closing this means extracting from a Chrome installer
  at build time: a decision about redistribution terms and a large build-time fetch,
  not a patch. Until then the Windows build says so once at startup rather than failing
  silently. The 4K software-decode concern in the same entry is untouched and still wants
  measuring on the real panel.

Still open, roughly in the order they are worth taking:

1. **G3 (partly done)** — the handlers and the recovery policy are in. `on_load_end` and
   `on_load_error` are observed firing; the policy is unit-tested. What is *not* verified
   is `on_render_process_terminated`: `chrome://crash` kills the renderer before CEF
   considers the browser created, and killing the render process by hand is unreliable to
   target (the zygote rewrites its forks' argv and this build sets no `CrRendererMain`
   thread name). Needs one run on the real box with a renderer killed under a live cast.
2. **G45/G49/G50** — the remaining Spotify items: queue-resolution retries, shuffle and
   repeat, local files.
3. **G48** — DIAL REST conformance (CORS, 200-on-relaunch, percent-decoding, `MX`).
4. **G53/G54** — the remaining test tiers: streaming error paths in `adapter_end_to_end`
   (SUSPEND→re-START, two phones at once, a bonded phone reconnecting), and a
   self-contained YouTube regression test.
5. **G30 (Windows half)** — a Widevine CDM for the deploy artifact, which means extracting
   from a Chrome-for-Windows installer: a redistribution decision, not a patch.

The AVRCP surface is now finished apart from browsing, which we deliberately do not claim.
What was grounded in BlueZ 5.86 rather than in memory, since a phone is the Target and its
behaviour is the one we have to match: the packet-type field's position and values, the
continuation request's parameter and the id its fragments carry, both continuation PDUs'
ctype, the Target's abort-on-other-PDU rule, the fixed five-byte notification-registration
parameter, the nine-byte `GetPlayStatus` layout, and the two category bits in attribute
0x0311.

G31's `HOME` assumption and G46's 4K decode both want checking on the real box rather
than more code.

---

## The three themes

Most of what follows is not 55 unrelated defects. It is three mistakes, each made
independently in three subsystems — which is worth naming, because fixing the instances
without fixing the pattern means making it a fourth time in `proto-airplay`.

### Theme A — nothing is supervised

Every subsystem terminates permanently on its first error, and the panel keeps looking
healthy. This is exactly the failure CLAUDE.md's ground rules and D30 name as the one to
design against ("every break lands as silence on an unattended panel"), and it is
currently true of all three protocols. D30's answer for Spotify — delegate upward to
librespot — does not cover it, because the reconnect is precisely the thing librespot 0.8
does not do (`librespot-core/src/session.rs:929`, a literal `// TODO: Optionally
reconnect`).

### Theme B — `Pipeline::control()` is a logging stub

`render_pipeline.rs:306` is `info!(?txn, "CONTROL (noted)"); Ok(())`. Two separately
documented features terminate there, so both protocols' control surfaces are complete,
tested, and inert.

### Theme C — preemption is one-directional, differently in each protocol

The session manager can background a source, but no protocol correctly gives up the
display or the audio device, and two cannot take it back afterwards.

---

## Tier 1 — silent, permanent, and reachable in normal use

- ✅ **G1 — The Spotify session is never supervised. CONFIRMED, silent.**
  `proto-spotify/src/session.rs:301`. `LiveSession` holds three `JoinHandle`s; nothing
  awaits or polls any of them, the only use is `.abort()` in `shutdown()`. Upstream
  `SpircTask::run` loops `while !self.session.is_invalid()` and simply *returns*, and
  librespot 0.8 does not re-establish the access-point session after a keepalive timeout.
  After an AP restart, a Wi-Fi blip, or ~130s of silence: castaway disappears from every
  phone's picker, the OSD says nothing, our logs say nothing, and the panel holds the last
  track forever. Recovery is a human re-pairing from the Spotify app.

- ✅ **G2 — Any Bluetooth transport error ends the stack for the process lifetime.
  CONFIRMED, silent.** `proto-bluetooth-audio/src/adapter.rs:329` does `warn!` then
  `return Ok(())`, so the caller cannot even distinguish failure from a clean stop;
  `app/src/bluetooth.rs:90` spawns it once with no supervisor and no re-enumeration.
  Unplug/replug, a USB reset, a wedged dongle, or G4 all produce the same silent death.
  The architecture doc names this as the lesson from the wedged UB500 — the STALL half was
  fixed, the exit-and-never-return half was not.

- 🟡 **G3 — No CEF lifecycle, load-error, or renderer-death handling. CONFIRMED, silent.**
  `pipeline/src/cef_browser.rs:489`: the `Client` provides render/request/display handlers
  and no `LifeSpanHandler`, no `LoadHandler` (`on_load_error`/`on_load_end`), no
  `on_render_process_terminated`. A sad-tab renderer crash freezes the last painted frame
  on the panel at `z=5` while DIAL still reports `running` and the screen id stays
  published. A `youtube.com/tv` that fails to load — DNS not up at boot, captive portal, a
  leanback deploy that breaks under our pinned TV UA — retries nothing and reports nothing.

- ✅ **G4 — The USB ACL read buffer is smaller than the packet we invite. CONFIRMED.**
  `hci-transport/src/usb.rs:42` sets `ACL_BUF = 1024`, but `adapter.rs:178` advertises a
  1017-byte L2CAP MTU, deliberately, so a full SDU lands in one ACL packet: 1017 + 4
  (L2CAP header) + 4 (HCI ACL header) = **1025**. The kernel's `btusb` uses 1028 for this
  exact reason. A bulk IN transfer larger than the request buffer is an overflow (usbfs
  `-EOVERFLOW`, and WinUSB is stricter still), and every non-`Stall` completion error is
  treated as fatal — so one maximum-size PDU from a phone triggers G2. Off by the four
  bytes of HCI header.

- ✅ **G5 — A preempted Spotify session can never play again. CONFIRMED, silent.**
  `proto-spotify/src/session.rs:278` emits `SessionEvent::Audio` with the PCM receiver
  exactly once, in `start()`. On preempt the receiver drops, `PcmSink::write` returns
  `NotConnected`, librespot calls `handle_pause()` and keeps running — but `Player::new`'s
  sink builder is `FnOnce`, so the channel cannot be rebuilt for the existing player, and
  there is no path to hand the pipeline a fresh `FrameSource::Pcm`. Ten seconds of
  Bluetooth, and from then on the Spotify device is still in the picker, still accepts
  play/pause, still updates the phone's UI, and is silent forever. The worst entry here,
  because it looks like it works.

- ✅ **G6 — The YouTube page never yields the screen. CONFIRMED.**
  `app/src/main.rs:119-129` is the only producer of `BrowserCommand`, both sends inside the
  DIAL closure; `BrowserRole::Fullscreen` sits at `z=5`, above video (`z=0`) and attract
  (`z=-10`). D28 itself records that nothing sends DIAL `DELETE` in practice. So after the
  first YouTube cast the leanback page owns the panel permanently: a DLNA/Cast/AirPlay
  video decodes and composites *underneath* an opaque page, Spotify plays under continuing
  YouTube audio (CEF audio goes straight to the system device — there is no `AudioHandler`
  and no route into the pipeline mixer), and attract mode never returns.

- ✅ **G7 — `SessionEvent::End` is never emitted by Spotify. CONFIRMED, silent.**
  `session.rs:401`'s `_ => false` swallows `PlayerEvent::SessionDisconnected` — which is
  exactly "the user pressed Disconnect on their phone" — and `shutdown()` tears down
  without telling the session manager. `core/src/session.rs:192` only clears the card on
  `End`. The user disconnects, and `SessionManager.active` stays `Spotify`,
  `pipeline.stop()` is never called, the card never clears, the display is never released,
  and the PCM thread keeps the audio device.

- ✅ **G8 — `proto-spotify`'s test target does not compile. CONFIRMED.**
  `session.rs:821,829,843,855,864` call `queue_from_cluster`, which the working tree
  deleted in favour of `queue_tracks` + `QueueNames::resolve`. The definition exists
  nowhere. The whole crate's test binary fails to build, so the blob-underflow guard, the
  `login_reason` and volume-scale tests are all currently not running either. The rewrite
  also silently dropped the `artist_name` metadata fallback that the old `queue_item` had,
  so queue rows now render the album where they used to render the artist.

---

## Tier 2 — wrong behaviour a sender will notice

- ✅ **G9 — AVRCP absolute volume is accepted and discarded. CONFIRMED (Theme B).**
  `adapter.rs:944` parses `SET_ABSOLUTE_VOLUME`, replies `Accepted`, emits
  `ControlTxn::Volume`, and `render_pipeline.rs:306` logs it. The phone's volume rocker
  does nothing. Worse if the phone enters absolute-volume mode on the strength of our
  category-2 Target record (`record.rs:353`) and stops attenuating locally — then playback
  is pinned at full scale with no working control on either end (that half SUSPECTED).

- ✅ **G10 — The panel's own control surface is unreachable by construction. CONFIRMED
  (Theme B).** `core/src/session.rs:99` `SessionManager::remote()` has zero callers outside
  core's own unit tests, and `app/src/main.rs:111` does `runtime.spawn(manager.run(event_rx))`
  where `run(self, …)` *consumes* the manager — so `remote()` cannot be called even in
  principle. All 156 lines of `proto-spotify/src/control.rs`, its capability negotiation
  and its `Stop → spirc.disconnect(true)` design, are dead at runtime, as is the
  `input-touch` path that module's doc comment describes. STATUS.md:22 claims the panel
  "drives back from the panel via `RemoteControl`".

- ✅ **G11 — Inbound AVRCP commands we do not model get no response at all. CONFIRMED.**
  `adapter.rs:842` returns early on three decode failures and `:968` is a bare `_ => {}`.
  Only `GET_ELEMENT_ATTRIBUTES` and `SET_ABSOLUTE_VOLUME` are ever answered. Dropped
  silently: `REGISTER_NOTIFICATION` *as a command* (the guard at `:898` only accepts
  `Interim`/`Changed`, so a phone subscribing to `EVENT_VOLUME_CHANGED` on our Target is
  ignored — `event::VOLUME_CHANGED` is defined and referenced nowhere), `GET_CAPABILITIES`
  (0x10), and every non-vendor AV/C opcode including `UNIT INFO` (0x30) and `SUBUNIT INFO`
  (0x31), which fail `VendorPdu::parse`'s ≥7-operand check. Nothing anywhere constructs a
  `Ctype::NotImplemented` or `Ctype::Rejected`. Every unsupported command costs the phone
  a full AVCTP transaction timeout, and stacks that gate bring-up on `UNIT INFO` or
  `GetCapabilities` — BlueZ-as-source does both — stall.

- ✅ **G12 — Only LDAC is gated on whether we can decode it. CONFIRMED, silent.**
  `app/src/bluetooth.rs:51` computes `enable_ldac` from `decodable_codecs()` and passes
  nothing else, despite the comment three lines above stating the table follows what the
  build can decode. `codec.rs:701` sorts aptX HD and aptX *ahead* of AAC and SBC, and
  senders take the first endpoint they support. A build whose ffmpeg lacks `aptx_hd`
  advertises it, the phone picks it, and the session is silence — the exact Q22 failure,
  fixed for one codec out of five. The invariant test at `audio_session.rs:493` asserts
  over `decodable_codecs()` rather than over what the adapter actually advertises, so it
  cannot catch this.

- ✅ **G13 — `dd.xml` carries no `<UDN>`. CONFIRMED (missing) / SUSPECTED (consequence).**
  `proto-dial/src/dial.rs:226` emits `deviceType`, `friendlyName`, `manufacturer`,
  `modelName` and stops. UPnP mandates `<UDN>`; Chromium's DIAL device-description parser
  treats an empty unique-id as a parse failure and drops the device, and Android senders
  use it to correlate the SSDP `USN` with the fetched description. The uuid is already
  threaded in via `ssdp_device(uuid)` — it is simply not rendered. Consequence: we may
  never appear in a Chromecast-family picker at all, while `curl` and `yt-selfplay` (which
  never read the UDN) both pass.

- ✅ **G14 — DLNA and DIAL advertise the same UUID. CONFIRMED.**
  `app/src/main.rs:299` and `:344` both derive from `config.uuid`, and
  `substrate-ssdp/src/device.rs:30` emits `upnp:rootdevice` plus the bare `uuid:` target
  for each device. An `M-SEARCH` for `ssdp:all` or `upnp:rootdevice` therefore gets two
  `200 OK`s carrying an identical `USN: uuid:…::upnp:rootdevice` with different
  `LOCATION`s. Control points that dedupe on USN — most do — pick one arbitrarily; pick
  the DLNA description and there is no `Application-URL` header, so DIAL is invisible. The
  targeted `ST: urn:dial-multiscreen-org:service:dial:1` path is unaffected, which is why
  nothing has caught it.

- ✅ **G15 — RECONFIGURE is accepted and ignored. CONFIRMED, silent.**
  `sink.rs:162` lumps `Reconfigure` in with `SecurityControl`/`DelayReport` and accepts
  with an empty payload: no state check (it is only legal in OPEN), no parse of the new
  capability, no `is_configuration()` validation, no codec-identity check, no
  `SinkEvent::Configured`, no new `Depacketizer`. The comment claims a sink "has no
  reconfigurable parameters", but the codec block is exactly what RECONFIGURE carries. A
  phone changing sample rate or bitpool mid-session (AOSP does this on a codec change from
  Developer Options, and some stacks do it on stream restart) gets an ACCEPT, switches its
  encoder, and we keep decoding at the old rate — wrong pitch or pure noise, nothing
  logged. Same failure class as Q25, through a door Q25 did not close.

- ✅ **G16 — Intel's loader ships AX200 firmware to AX210/AX211. CONFIRMED.**
  `hci-transport/src/init/intel.rs:100` fixes `image_stem: "intel/ibt-20-1-3"` on the
  loader rather than deriving it from the TLV version the way `btintel.c` does, while
  `:115` lists AX210 (`0x0032`) and AX211 (`0x0033`) among its products. AX210 needs
  `ibt-0041-0041.sfi` — which `flake.nix:100` **already embeds**, unused. The wrong signed
  image goes to a secure-boot part, which rejects it or accepts a partial upload;
  `required_images()` reports the wrong file too, so the probe's MISSING check lies.
  Related, same file: after `INTEL_RESET` we do not wait for the vendor boot event before
  `LOAD_DDC`, contradicting our own table in architecture-substrate.md §11.3a (SUSPECTED).

- ✅ **G17 — Realtek hardcodes one chip's firmware for every part it claims. CONFIRMED.**
  `realtek.rs:66` fixes `rtl8761bu_fw.bin`/`_config.bin`, while `:35` names 8723B/8821A/
  8822B/8852A/8703B and `:92` claims `0BDA:8761`, `0BDA:A725`, `0BDA:B00A`. `btrtl.c`
  selects on `lmp_subver` **and `hci_rev`** — `0x8761` with `hci_rev 11` is
  `rtl8761b_fw.bin`, with `hci_rev 12` is `rtl8761bu_fw.bin`. We read `hci_rev` into
  `LocalVersion` and only log it. A bare RTL8761B or an 8723B gets another chip's patch;
  the download succeeds command-by-command and the radio then misbehaves or is bricked —
  which the file's own comments name as the worst case.

- ✅ **G18 — No L2CAP RTX/ERTX response timers. CONFIRMED, silent.**
  `substrate-l2cap/src/mux.rs:452`: `next_timeout` consults only `self.ertm`. No signalling
  request has a timer, so a `ConnectionRequest` or `ConfigurationRequest` the peer never
  answers leaves the channel in `WaitConnectRsp`/`WaitConfig` forever — no retransmission,
  no `ChannelClosed`, the CID never freed, the caller never told. Spec §6.2.1 requires RTX
  (1–60s) and a teardown. This bites the channels *we* dial: `adapter.rs:1006` sets
  `link.art_sdp = Some(…)` before the response arrives and `open_cover_art` returns early
  forever while it is `Some`, so a phone that ignores our SDP or AVCTP connect leaves cover
  art and the outbound AVRCP control channel permanently dead for that link.

- ✅ **G19 — No `CommandReject` is ever constructed, and one bad command discards the whole
  C-frame. CONFIRMED.** `mux.rs:487`/`:632`, `signaling.rs:633`. An unknown signalling code
  (`Create Channel 0x0C`, `Move Channel 0x0E`, anything future) or a truncated command
  makes `Signal::decode_all` return `Err`, and `handle_pdu`'s `?` discards **every** command
  in that PDU — including well-formed ones packed alongside, the very packing the crate
  went to the trouble of supporting. Inbound `CommandReject` from the peer is also swallowed
  with no action, so a phone rejecting our configuration request leaves us waiting forever,
  compounding G18.

- ✅ **G20 — Screen-id resolution is a one-shot 60s window with no cancellation. CONFIRMED.**
  `app/src/main.rs:364`, `app/src/screen.rs:31`. Two defects in the D28 fix: each launch
  spawns `publish_screen_id` with no handle and no generation counter, so a relaunch inside
  60s leaves the old task polling the old pairing code and whichever finishes last wins —
  the stale writer can overwrite the fresh screen id or refill a slot `stop` just cleared,
  reproducing the exact D28 symptom the slot exists to prevent. And 20 × 3s then permanent
  silence: if the page needs longer (cold boot, slow link, G23), the id is never published
  for that launch and nothing re-attempts. `screen.rs:69` also uses a bare `ureq::post`
  with **no timeout**, unlike `filterlists::fetch`, so one hung attempt eats the budget.

- ✅ **G21 — The SponsorBlock receive channel corrupts itself on the first multi-byte
  character and never recovers. CONFIRMED, silent.** `app/src/sponsorblock/actor.rs:233`.
  Three compounding problems: `from_utf8_lossy` on an arbitrary 8192-byte read boundary
  replaces a split UTF-8 sequence with U+FFFD, and the Lounge framing is *character*-counted,
  so one split multi-byte char — a phone named with an emoji in `loungeStatus`'s device
  list is the common case — permanently desynchronises every subsequent length prefix. On
  a parse error the code `continue`s **without clearing `buffered`**, so it re-parses the
  same corrupt prefix forever while the buffer grows unbounded. And `ureq::get(&url).call()`
  uses the default agent with no read timeout, so a silently-dropped connection parks the
  blocking thread in `read()` indefinitely, `commands.recv()` never errors, and the reattach
  path at `:45` never fires. Sponsor skipping and `skipAd` stop for the rest of the process
  lifetime, with one `warn!` at most.

---

## Tier 3 — degraded quality, conformance, and diagnosis

- ✅ **G22 — 160 kbps and no loudness normalisation, both by silent default. CONFIRMED.**
  `session.rs:234` spreads `PlayerConfig::default()`, where upstream `Bitrate::default()`
  is `Bitrate160` and `normalisation: false`. `app/src/config.rs:153` exposes only
  `initial_volume`. A Premium account entitled to 320 plays at 160 on the PA, with
  track-to-track loudness jumps every real Connect speaker smooths out, and neither is
  discoverable from any log line.

- ✅ **G23 — Startup blocks the main thread on a 2.7 MB fetch + QuickJS eval before the
  browser exists. CONFIRMED.** `app/src/main.rs:169`, `filterlists.rs:51` (90s budget),
  `ubo_scriptlets.rs:36` (20s budget). `serve()` is spawned *first*, so DIAL answers `201
  Created` and `<state>running</state>` and posts "Launching YouTube…" while
  `load_or_fetch_all` still blocks the main thread — worst case ~110s before
  `cef.initialize()`. The `Navigate` queues on an unbounded mpsc so the page does load, but
  G20's 60s budget may already have expired: a phone connects, sees `running`, and can
  never queue anything. The `SharedBlocker` cell already supports swapping the engine in
  later, so this belongs on the blocking pool with the browser coming up on the built-in list.

- **G24 — Delay reporting is advertised but never sent. CONFIRMED.**
  `avdtp.rs:329` + `sink.rs:186` advertise the capability in GET_ALL_CAPABILITIES and the
  SDP record claims A2DP 1.3 (`record.rs:246`), but nothing ever sends a `DELAY_REPORT`
  command — the only `Signal::DelayReport` arm is the accept-and-ignore at `sink.rs:165`,
  handling a direction that never occurs (DELAYREPORT is SNK→SRC). The phone believes sink
  latency is zero, so video watched through this speaker is out of lip-sync by the whole
  decode + cpal buffer depth (250 ms `LEAD` plus the ring) with no way to compensate. Also
  a conformance failure against the profile version we publish.

- ✅ **G25 — AVRCP vendor-PDU fragmentation is not handled. CONFIRMED (code), SUSPECTED
  (frequency).** `avrcp.rs:200`: `VendorPdu::parse` reads `operands[3]` (pdu_id) and
  `operands[5..7]` (length) and never looks at `operands[4]`, the packet type
  (0=single/1=start/2=continue/3=end). There is no `REQUEST_CONTINUING_RESPONSE` (0x40) or
  `ABORT_CONTINUING_RESPONSE` (0x41) anywhere. A fragmented `GetElementAttributes` response
  parses the start fragment as the whole thing, `parse_element_attributes` returns
  `Truncated`, and `adapter.rs:870`'s `if let Ok(parsed)` discards it silently — so on any
  phone whose metadata exceeds the AVCTP fragmentation threshold (long or CJK titles, all
  7–8 attributes) the now-playing card stays permanently blank. The outbound side truncates
  at 450 bytes (`avrcp.rs:274`) rather than fragmenting, which is deliberate and documented
  but clips the card a head unit asks us for.

- ✅ **G26 — Playback position never moves. CONFIRMED.**
  `avrcp::get_play_status()` (`avrcp.rs:341`) and `event::PLAYBACK_POS_CHANGED`
  (`avrcp.rs:46`) are referenced from nowhere outside their own unit tests; only
  `PLAYBACK_STATUS_CHANGED` and `TRACK_CHANGED` are registered (`adapter.rs:563`). Track
  duration arrives via attribute 7, but elapsed position never does, so the scrubber sits
  at zero for the whole track. Q28's subscription half landed and the position half did
  not, while §11.4 of the architecture doc still claims position.

- ✅ **G27 — The now-playing card re-rasterizes at full resolution once per second for a
  position it never draws. CONFIRMED.** `session.rs:46` sets `POSITION_INTERVAL = 1s`,
  justified as keeping the card's progress honest. Each `PositionChanged` sets
  `changed = true` (`:382`) → `NowPlaying` → `publish_card` → `nowplaying_card::render` at
  `card_size()` = the compositor target, then a full texture upload. But
  `nowplaying_card.rs` contains no progress bar and no time — the only `position` hits are
  `Iterator::position` in tests. A full 4K RGBA rasterize plus GPU upload every second,
  forever, buying nothing.

- **G28 — No HCI command-credit accounting and no command timeout. CONFIRMED.**
  `substrate-hci/src/event.rs:140` parses `allowed_packets` and nothing consumes it outside
  a test. Bring-up has no watchdog: `advance_bring_up` fires only from a Command Complete
  or Status, so a lost completion — the documented idle-stall on this exact dongle — stops
  the queue with no `Ready`, no `WriteScanEnable`, and no log line, forever. The firmware
  loaders bound this with a 5s timeout; `HostController` has nothing equivalent. Runtime
  commands are also unpaced: each event handler sends immediately, so two events back to
  back put two commands in flight while most controllers advertise
  `Num_HCI_Command_Packets = 1` (SUSPECTED at runtime; a dropped pairing reply during a
  two-phone connect storm presents as one phone hanging in "Connecting…").

- ✅ **G29 — ACL credits leak when a write is queued for a handle that has just dropped.
  CONFIRMED.** `proto-bluetooth-audio/src/acl.rs:116`/`:130`. `link_down` reclaims
  outstanding credits but neither purges nor filters jobs already queued for that handle;
  the woken `pump` then calls `claim(dead_handle)`, re-inserting an outstanding entry that
  no `Number_Of_Completed_Packets` will retire and that `link_down` — already fired — will
  never reclaim. Each occurrence permanently shrinks the pool, so with `acl_credits=6` on
  the deploy dongle six such teardowns wedge the writer for the process lifetime: phones
  connect, get an L2CAP connection response, and then nothing. The existing test
  (`adapter_end_to_end.rs:533`) drops a link with nothing queued, so it does not reach this.

- 🟡 **G30 — Vendor coverage is Intel + Realtek only, and an unknown dongle silently gets
  `NoInit`. CONFIRMED, silent.** `hci-transport/src/init/mod.rs:114`. No Broadcom
  (`hci_bcm` `.hcd` patchram), no MediaTek (MT7921/7922, extremely common in current
  dongles), no Qualcomm/Atheros. `registry_strict()` exists and is tested but is **never
  called from production code** — the runtime path always takes the catch-all. On Windows,
  where nothing else loads firmware, plugging in a Broadcom or MediaTek dongle logs
  "initialising controller loader=rom", `NoInit` reports success, `HCI_Reset` may even be
  answered by the bootloader, and the radio is inert. `open_first()` (`usb.rs:175`) also
  takes whatever HCI-class device enumerates first, with no preference for one we have a
  loader and firmware for.

- ✅ **G31 — The filter-list cache directory is almost certainly unwritable under the shipped
  NixOS module. SUSPECTED (high confidence), silent.** `filterlists.rs:78` resolves
  `XDG_CACHE_HOME` → `HOME/.cache` → `temp_dir()`, while `flake.nix:573` sets only
  `CASTAWAY_CONFIG` and `RUST_LOG` with `DynamicUser=true`, `ProtectSystem=strict`,
  `ProtectHome=true`. A dynamic user's home is `/`, so the path is `/.cache/castaway` —
  unwritable, with every failure swallowed (`let _ = create_dir_all`, `warn!` on write).
  The render process is where it bites: `render_blocker()` calls `load_cached_only()`, which
  returns `None` with no cached list, so `on_context_created` returns early and **no
  `##+js(…)` scriptlet is ever injected in the deployed configuration** — while the browser
  process still blocks network requests from its in-memory engine, so the receiver looks
  healthy. Exactly the silent failure Q17/Q36 were written to prevent, reintroduced by the
  deployment environment. The same base path feeds `stable_cache_dir()`, so the CEF profile
  — cookies, "watch as guest" — is also non-persistent. Fix is `XDG_CACHE_HOME=%C` or
  `CacheDirectory=castaway` in the unit; worth confirming `HOME` on the box first.

- ✅ **G32 — A daily refresh that fetches a >1 KB non-list body destroys the good cache.
  CONFIRMED, silent.** `filterlists.rs:368`: `text_for` accepts any response over 1024
  bytes, writes it to the cache, and builds the engine from it. A captive-portal
  interstitial, a Cloudflare challenge, or a GitHub error page all clear that bar. The good
  cached list is overwritten on disk, the in-memory engine is swapped for one built from
  HTML, and render processes rebuild from the poisoned cache on the next stamp change. Ad
  blocking degrades to nothing and stays there across restarts until a clean fetch
  succeeds. The 1024-byte heuristic anticipates the 404-page case but not large garbage; a
  content sniff (`[Adblock` header, or a `||`-rule ratio) closes it.

- ✅ **G33 — Every SponsorBlock reattach is a brand-new remote control on the user's screen.
  CONFIRMED.** `app/src/sponsorblock/actor.rs:57`: `session()` mints a fresh
  `Uuid::new_v4()` device id and lounge token every cycle, and the cycle ends whenever the
  long poll returns EOF, which BrowserChannel does routinely. The Lounge's connected-device
  list accumulates "castaway SponsorBlock" entries (visible in the phone's cast UI, and
  YouTube may show connect toasts on screen); `Planner`, `PlaybackClock` and `ad_skip_sent`
  are discarded and segments re-fetched each time; and there is a fixed 5s dead window plus
  a token round trip during which a segment boundary is simply missed. A stable per-device
  id and an in-place resume — the `Bound` typestate already tracks `aid` for exactly this —
  fixes both.

- ⛔ **G34 — Manual TV-code pairing is unreachable. CONFIRMED.**
  `app/src/main.rs:180`, `pipeline/src/kiosk.rs:61`.
  hackerspace-receiver-build.md:79 names two pairing modes, same-LAN DIAL and cross-network
  TV code (`youtube.com/pair`); only the first exists. At idle the browser shows
  `attract_widget_url`; `youtube.com/tv` is loaded only by a DIAL launch. And `route_input`
  handles cursor/wheel/touch but **not `WindowEvent::KeyboardInput`**, so even with the page
  up there is no D-pad/Enter to drive leanback's 10-foot UI to the code screen. A guest on a
  different VLAN or guest-isolated Wi-Fi has no path to cast at all.

- ✅ **G35 — `activeUser` is set before login is attempted and never cleared. CONFIRMED.**
  `proto-spotify/src/lib.rs:159` sets it immediately after blob decryption, *before*
  `handle.paired()`, and nothing resets it when `session::start` fails (`session.rs:171`
  only logs and banners) or when the session dies. A non-Premium account or a stale blob
  leaves `getInfo` reporting `"activeUser": "alice"` forever — and `pairing.rs:93` documents
  `getInfo` as what a phone reads back to decide whether the device is *yours*, so the phone
  concludes it is connected to a device with no session. A second person also sees the first
  person as active after the first person's login failed.

- ✅ **G36 — `run_pcm` cannot observe `stop` while parked. CONFIRMED.**
  `pipeline/src/audio_session.rs:157` is `while let Ok(block) = frames.recv()` with the
  `stop` check inside the body. Preempted while Spotify is *paused*, the thread blocks in
  `recv()` forever and never reaches `output.stop()` at `:185`, so `CpalAudioOut` keeps its
  stream thread parked with the device open. A leaked thread and a held device per
  preempt-while-paused; on a box with an exclusive-mode ALSA device the next source's
  `start()` then fails outright.

- 🟡 **G37 — Bluetooth preemption is advisory only. CONFIRMED.**
  `adapter.rs:684`: the losing phone gets an AVRCP `PAUSE` passthrough, but no AVDTP
  `SUSPEND` is sent and `session_open`/`audio_tx` stay live, so its media packets keep
  flowing into a backgrounded session. A phone with no AVRCP channel, or one that ignores
  the keypress, keeps streaming and keeps burning airtime and ACL credits against the phone
  that actually won. Q27 records two phones fighting over one output; pausing the player
  rather than stopping the stream leaves the underlying condition intact.

- ✅ **G38 — The card shows the raw Spotify canonical username. CONFIRMED.**
  `session.rs:287` uses `user.user_name`, the `addUser` form field, which for any account
  created since roughly 2015 is a 25-character random string. The panel renders
  `Connected — 31l6zbn3kq7wxyz…` at 28px on a 65-inch screen. `PlayerEvent::SessionClientChanged`
  carries `client_name`/`client_brand_name`/`client_model_name` — the actual phone, the best
  possible display name — and is discarded by the same wildcard as G7.

- ✅ **G39 — `ControlCapabilities` is not derived from the peer's AVRCP features. CONFIRMED.**
  `adapter.rs:536` always builds `AvrcpControl::passthrough(tx)`. The peer's
  `SUPPORTED_FEATURES` *is* fetched — `Query::avrcp_target` asks for the full attribute
  range — and only `cover_art_psm()` is read from the response. Architecture §11.5 states
  capabilities are "populated from the peer's AVRCP supported-features bitmask, so the UI
  cannot offer a button the phone will reject"; they are not. The panel offers
  play/pause/next/prev/stop/mute for every phone, the button is pressed, an AV/C
  `NOT_IMPLEMENTED` comes back, and nothing in the UI reflects it.

- ✅ **G40 — AAC with `numSubFrames > 0` fails every packet, silently. CONFIRMED.**
  `latm.rs:201` refuses multi-subframe multiplexes and `adapter.rs:829` logs the resulting
  `BadMediaPacket` at `debug!`. A sender that packs several access units per
  `AudioMuxElement` produces a connected phone, a running session, a populated now-playing
  card, and total silence with nothing at default log level. The general form is the worst
  diagnostic hole in the media path: **`on_media` has no counter and no rate-limited warning
  for sustained depacketize failure**, only `debug!`.

- ✅ **G41 — Authentication and encryption events are parsed and dropped. CONFIRMED.**
  `proto-bluetooth-audio/src/host.rs:390`'s `_ => Vec::new()` swallows
  `AuthenticationComplete` and `EncryptionChange`. We never call `AuthenticationRequested`
  or `SetConnectionEncryption` (both `Command` variants exist, unused), never require
  encryption before serving AVDTP, and never react to an authentication *failure* — so a
  stale stored link key produces a silent connect/disconnect loop with nothing logged beyond
  `link down`, and the stale key is never evicted from `link_keys` (`:339`). SUSPECTED
  severity: usually self-heals when the phone re-initiates SSP and `LinkKeyNotification`
  overwrites, but it is invisible while it lasts.

- ✅ **G42 — Configuration continuation (the C-bit) is parsed and ignored. CONFIRMED (code),
  SUSPECTED (frequency).** `mux.rs:567` destructures `flags` away and every response is
  built with `flags: 0`. A peer sending a multi-part `ConfigurationRequest` (C=1) gets a
  `Success` for a partial option list and `incoming_done` is set, so we open the channel
  while the peer is still sending options. C=1 is uncommon but legal, and the result is the
  classic "connects, then the first data PDU is dropped".

- ✅ **G43 — Abandoned channels are not torn down toward the peer. CONFIRMED.**
  `mux.rs:985` `fail_configuration` removes the channel locally and emits `ChannelClosed`
  but sends no `DisconnectionRequest` — while `fail_channel` at `:532` does. The phone keeps
  a half-open channel until its own RTX gives up, and a retry then collides with a CID we
  consider free.

- ✅ **G44 — Undecodable AVDTP signaling gets no reply. CONFIRMED (low-medium).**
  `adapter.rs:708`: a `Message::decode` failure logs at `debug!` and returns, covering
  unknown signal ids and *any fragmented AVDTP signaling packet*. The peer waits out its
  signal timeout, retries, and typically aborts the link. Low risk in practice because our
  1017-byte receive MTU gives peers no reason to fragment, but the correct answer — a
  GENERAL REJECT carrying the transaction label — is never emitted (`MessageType::GeneralReject`
  is never constructed anywhere).

- **G45 — Queue name resolution can become a per-update RPC storm, awaited inline.
  SUSPECTED.** `session.rs:513`: `QueueNames::resolve` inserts into `self.seen` only on
  success, so a track whose `AudioItem::get_file` fails (region-locked, transient 5xx) is
  retried on every cluster update, up to `RESOLVE_LIMIT = 6` sequential awaited round trips
  per update, inline in `pump_queue`'s loop — and cluster updates arrive on volume changes,
  device-list churn, and hand-offs. `seen` is also never evicted, so a long shared-panel
  session accumulates a `QueueItem` per URI ever queued (certain, but slow).

- ✅ **G46 — DRM, 4K, and codec reality on the browser path. SUSPECTED.**
  `cef_browser.rs:266` sets `disable-gpu` + `disable-gpu-compositing`, so **all decode is
  software** and every frame is a CPU `on_paint` copy of the full viewport — at 3840×2160
  that is ~33 MB per frame before the compositor upload, with `windowless_frame_rate: 60`.
  4K60 will not keep up; the dirty-rect accumulator helps, but the video rect *is* most of
  the frame. No Widevine CDM is packaged or configured anywhere in the tree, so EME-gated
  content will not play, surfacing only as a leanback console error on `castaway::console`.
  Neither is recorded as a known limit. Worth measuring before the deploy rather than
  discovering on the wall.

- **G47 — Blocking filesystem I/O on the tokio runtime. CONFIRMED (ground rule 4).**
  `adapter.rs:492` invokes `on_paired` synchronously inside `apply_host_action`, and
  `app/src/bluetooth.rs:66` → `store_link_key` (`:243`) does `read_to_string` + `File::create`
  + writes on that thread — parking the adapter actor, and with it the media channel and the
  ACL reader, for a disk round trip at the most timing-sensitive moment of a new connection.
  `Firmware::File` → `std::fs::read` (`firmware.rs:32`) is likewise called from inside the
  async `init()` of both loaders.

- **G48 — DIAL REST conformance details. CONFIRMED.**
  No CORS headers anywhere (`dial.rs:198`), which DIAL 2.1 requires of the REST service
  (`Access-Control-Allow-Origin`, exposing `Location`) — browser-based senders fail, native
  phone apps do not care. `POST` always returns 201 (`:303`) where the spec wants 200 on a
  relaunch, with no `Content-Type` check and the entire raw body interpolated into the
  navigation URL bounded only by axum's 2 MB default. `form_field` (`:331`) does not
  percent-decode, only `+`→space, so a pairing code with an escaped character resolves the
  wrong screen (latent — today's senders use plain alphanumerics). SSDP parses `MX` and
  ignores it (`responder.rs:120` replies immediately) and carries no
  `BOOTID.UPNP.ORG`/`CONFIGID.UPNP.ORG`.

- **G49 — Shuffle and repeat are unmodelled end to end. CONFIRMED.**
  `core/src/event.rs:50`'s `ControlTxn` has no `Shuffle`/`Repeat`, and librespot's
  `ShuffleChanged`/`RepeatChanged` hit the G7 wildcard. Playback is unaffected — spirc
  honours the phone internally — so this is a panel display/control gap, not a silent
  playback failure. Noted so it is not mistaken for one.

- **G50 — Local-file tracks are rendered but cannot play. SUSPECTED.**
  `session.rs:671` carefully renders `UniqueFields::Local`, but `PlayerConfig::default()`
  leaves `local_file_directories` empty and there is no config surface. A playlist with
  synced local files gets a correctly-rendered card for a track that emits
  `PlayerEvent::Unavailable` and skips. Arguably correct for a receiver that holds no user
  files — but the metadata handling implies otherwise.

---

## Test-coverage gaps (why none of the above was caught)

- ✅ **G51 — No test asserts that a single audio sample ever left the box, for any protocol.**
  `proto-spotify/examples/selfplay.rs:497` polls `GET /v1/me/player` and asserts
  `is_playing == true` — a field echoed back from the device's *own* connect-state PUT,
  i.e. from spirc's state machine. Nothing asserts `PcmSink::write` was called, that
  `run_pcm` opened an output, or that `NullAudioOut::frames()` advanced. Both real bugs the
  current working diff is fixing (a `blocking_send` panic on the first block, and
  turbo-advance pacing) live entirely below the layer `selfplay` observes.

- ✅ **G52 — `substrate-l2cap::mux` has no `#[cfg(test)]` module at all** (1048 lines); all
  coverage is `tests/handshake.rs`, two cooperating multiplexers on the happy path. Not
  covered anywhere: a peer that never answers (there is no timer to test — G18), a
  `CommandReject`, an unknown signalling code mid-PDU, `ConfigResult::Rejected`, exceeding
  `MAX_CONFIG_ATTEMPTS`, link loss mid-configuration, a config request for an unknown CID,
  or any fuzz of random bytes into `handle_pdu`. `ertm.rs` is the exception and is well
  covered on error paths.

- **G53 — `adapter_end_to_end.rs` (1511 lines) is thorough about bring-up and silent about
  streaming.** One full aptX stream that pushes **exactly one media packet**. Never
  exercised: SUSPEND→re-START, CLOSE→reconfigure, RECONFIGURE at all, ABORT over the wire,
  two phones streaming concurrently, inbound `SET_ABSOLUTE_VOLUME`, inbound
  `REGISTER_NOTIFICATION`, any malformed or short media packet reaching `on_media`,
  audio-queue overflow, a bonded phone reconnecting after restart, or SBC end to end (the
  only end-to-end codec is aptX).

- **G54 — No automated test drives a YouTube cast end to end.** The tier-2 VM test
  (`nix/vm-test.nix:456`) runs a browser-less build and asserts DIAL's *absence*. Everything
  positive lives in `nix run .#yt-selfplay`, which needs the real internet and a human, and
  whose `--reconnect` mode additionally requires the operator to have launched a cast by
  hand first — so the D28 regression test is not self-contained. Not covered: `DELETE` →
  surface dismissal, two senders on one screen, pause/seek/volume from the sender,
  add-to-queue, a relaunch inside the screen-id window (G20), or `--expect-skip` over a
  channel that has reattached.

---

## Documentation drift

STATUS.md is a 2026-07-23 snapshot: it lists 16 crates where there are now 21, and the
entire Bluetooth stack landed after it was written. Beyond staleness, three specific claims
are contradicted by the code: STATUS.md:22 says Spotify "drives back from the panel via
`RemoteControl`" (G10); architecture-substrate.md §11.4 claims AVRCP playback position
(G26); §11.5 claims `ControlCapabilities` is populated from the peer's feature bitmask
(G39). Q24 says we "mirror `SetAbsoluteVolume` into the pipeline's gain" (G9).

---

## What is solid

Stated so the list above can be read in proportion — these were examined and found good.

**Spotify.** The pairing crypto (`crypto.rs`) is cross-checked against librespot's *own*
decoder rather than only round-tripped, which is a real second opinion. The `< 16` byte
guard genuinely prevents the upstream `0..len - 0x10` underflow. The Q38 dealer-subscription
reasoning is correct — `retain` over a `Vec` of subscribers with an unbounded channel means
subscribing alongside spirc really is supported and a slow `pump_queue` cannot stall spirc.
The `Some(&[])` vs `None` queue distinction is the right fix. Both mid-flight fixes in the
working tree are correctly reasoned, and dealer/token reconnection *is* handled upstream —
the gap is one layer down, at the AP session (G1).

**YouTube.** The DIAL launch/stop state machine is well tested at the router level and the
D28 `ScreenSlot` fix is real. The SponsorBlock pure core is the best-tested code in the
audit: hash-prefix privacy, overlap merge, NaN/negative rejection, skip-once memory, rewind
re-arm with a settle window. The Lounge sender typestate's RID/ofs/AID advancement is
correct. The ad-flag reading is taken from a real capture and gets the string-typed
`"true"`/`"false"` and `isSkipEnabled`-vs-`isSkippable` distinctions right. Letting the
leanback page own queue manipulation, multi-sender, and volume is a sound architectural
choice with no seam problems found.

**Bluetooth.** ACL transmit flow control is genuinely good — credits per handle,
over-reported completions clamped, buffers reclaimed on link loss, one writer so fragments
never interleave, tested both ways (the leak in G29 is a narrow case around it, not a design
flaw). HCI framing is parse-don't-validate and tolerates USB padding, truncation, reserved
handle bits, and non-UTF-8 remote names. ERTM is real and well covered on error paths: SAR
with the start-frame adjustment, CRC-16 tested against the published check value, REJ once
per gap, poll⇄final, wrap-safe modular sequence arithmetic. The mode-negotiation asymmetry
("listener holds, dialler adapts") is right. Both firmware loaders get the genuinely hard
parts right — Realtek's epatch container and 7-bit wrapping index, Intel's secure-boot
ordering and the legacy-vs-TLV `Read_Version` trap — and both bound time rather than
iterations and refuse to re-flash a working part; G16/G17 are about *selection*, not
mechanism. On the profile side: capability encode/decode for all five codecs including the
LE vendor-id trap and AAC's 12-bit split rate field, per-signal reject payload shapes,
per-codec media framing, the LATM parser against a real iPhone fixture, RTP timestamp
rebasing with wraparound, the OBEX/BIP cover-art session, and the SDP server's stateless
continuations.

**Across all three:** no `unwrap`/`expect`/`panic!` on any runtime-reachable path in any
audited crate — every hit is inside `#[cfg(test)]`. `unsafe` is confined to `socket.rs` with
per-block SAFETY comments. Ground rule 3's sans-I/O separation genuinely holds. Ground rule
4 holds except for G47 and G23.
