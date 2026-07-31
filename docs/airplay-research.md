# AirPlay — research record

What five parallel research passes established about AirPlay on 2026-07-27, grounded in
reference implementations cloned at HEAD (UxPlay `acfb549`, shairport-sync `d6ac53b`,
airplay2-receiver `6c343d3`, pyatv `b277a4c`, RPiPlay `64d0341`) and in real captured
advertisements from `openairplay/airplay-spec`.

Structured like `dlna-conformance.md`: claim → evidence → confidence. Where the reference
implementations disagree, that is recorded as a disagreement rather than resolved by
picking a favourite.

**This is a research record, not a plan of record.** §2 ends in a decision that is not mine
to make.

---

## 1. What exists today

`proto-airplay` is ~780 lines and terminates nothing. It advertises, answers `/info`, and
runs an RTSP dispatch that returns `200 OK` to `ANNOUNCE`/`SETUP`/`RECORD`/`SET_PARAMETER`
without doing anything with them, and `501` to `/pair-setup`, `/pair-verify`, `/fp-setup`.
`crypto-fairplay` is a three-state typestate whose every derivation step returns
`NotImplemented`.

Media plane: none. Mirroring: none. There is no RTP receive path, no ALAC/AAC decode wiring,
no mirror data channel, no timing, no event channel, and no AirPlay Video endpoints.

Integration coverage does exist — `nix/vm-test.nix:388-467` drives a scripted Python sender
against both ports and asserts the `501`. Multicast already works across the nixosTest
bridge; `avahi-browse` from the sender node already asserts our TXT records. The harness is
not the gap.

---

## 2. Three regimes, and the fork

There is not one AirPlay auth flow. There are three, and **the receiver chooses which one it
faces by what it advertises**. This is the single highest-leverage fact in this document.

| Regime | Endpoints | Crypto | Selected when | Control channel after |
|---|---|---|---|---|
| **No pairing** | *(none)* → straight to `/fp-setup` | — | bit 27 off **and** bits 38/43/46/48 off | plaintext RTSP |
| **Legacy pairing** | `/pair-setup`, `/pair-verify` | X25519, Ed25519, AES-128-CTR, SHA-512 KDF | bit 27 on, no HomeKit bits | **plaintext** (the ECDH secret only re-keys the media AES key) |
| **HomeKit transient** | `/pair-pin-start`, `/pair-setup` (`X-Apple-HKP: 4`) | SRP-6a SHA512/3072, PIN `3939`, M1–M4 only | bit 48 (and/or 43) | ChaCha20-Poly1305 framed |
| **HomeKit full** | `/pair-setup` M1–M6 (`X-Apple-HKP: 3`), then `/pair-verify` | + Ed25519 long-term keys persisted | bit 46 | ChaCha20-Poly1305 framed |

`X-Apple-HKP`'s presence/absence is the cleanest wire discriminator: legacy pairing sends no
such header (`pyatv/protocols/airplay/auth/legacy.py:20-23`), and `/pair-verify` accepts only
`X-Apple-HKP: 3` (`pyatv/protocols/airplay/server_auth.py:232-242`). Values 2 and 6 could not
be confirmed from any source — model the header as an enum with an explicit `Unknown(u8)`
variant rather than assuming exhaustiveness.

### 2.1 The fork

The two viable targets want **opposite advertisements** and have **opposite cost structures**.

| | **Path A — AP1 audio** | **Path B — legacy mirroring** | **Path C — AP2 audio** |
|---|---|---|---|
| Model | shairport-sync classic | UxPlay | shairport-sync AP2 |
| Advertise | `_raop._tcp`, `et=0,1`, `cn=0,1`, no AP2 bits | `0x527FFEE6,0x0`, `et=0,3,5` | `0x405C4A00,0x18340`, `et=0,1` |
| Pairing | none | none (bit 27 off) | HomeKit transient (~800–1000 lines) |
| FairPlay | none | **OmgHax required** — it is the whole ballgame | table lookup only; key is `shk` from SETUP |
| Licence question | none | **yes** (see §5.3) | none |
| Media key from | RSA-unwrapped `rsaaeskey` in SDP | `fairplay_decrypt(ekey)` | `shk`, already inside the encrypted channel |
| Gets you | sound from an iPhone | **the headline gesture** | sound, no licence exposure |
| Mirroring possible? | no | yes | **nobody in open source has ever done it** |

Path A is the cheapest thing that works at all. Path B is the thing people actually want from
a panel — "Screen Mirroring" in Control Center *always* takes this path. Path C is Path A's
grown-up sibling and buys nothing a panel needs that A doesn't, at much higher cost.

**These are not mutually exclusive over time, but they are mutually exclusive in a single
advertisement.** A receiver cannot simultaneously tell iOS "I do HomeKit transient pairing"
and "I do no pairing at all". Sequencing A → B means changing the advertisement between
milestones, which is cheap; trying to serve both at once is not.

**Decided: A first.** The advertisement, `/info` and the `/fp-setup` handshake are done;
what follows is §7's Path A build order. Path B is not abandoned — nothing landed for A
forecloses it, and the bit to set when mirroring works is named in `advert.rs` — but it
carries a licence decision (§5.3) that should be made deliberately rather than arrived at
by writing code. Path C buys a panel nothing that A does not.

---

## 2.2 Climbing the ladder: do AirPlay 1 and 2 coexist?

**Yes, on one device, and that is what real devices do.** The Denon AVR-X3500H, Sonos
Symfonisk, Apple TV 4K and HomePod captures all advertise `_airplay._tcp` and
`_raop._tcp` at once, and shairport-sync's AirPlay 2 build still serves AirPlay 1 — its
release notes call it "the Classic AirPlay feature of AP2".

Three facts make coexistence work:

1. **The sender picks the generation from the advertisement.** pyatv's
   `get_protocol_version` keys AP2-vs-AP1 off features bit 38 or 48
   (`utils.py:241-256`), and owntone additionally gates buffered audio on
   `srcvers >= 354.54.6` and PTP on `srcvers >= 366` (`airplay.c:419-420`).
2. **The flows diverge at the sender's first move and never interleave.** AirPlay 1
   opens with `ANNOUNCE` + SDP; AirPlay 2 opens with `SETUP` + a binary plist. So a
   receiver dispatches on what arrives, not on a configured mode — which is why
   `actor.rs` deliberately does not let the socket decide which media plane a session is.
3. **The one thing that is not additive is the `_raop._tcp` TXT dialect.** Classic
   carries `txtvers`/`ch`/`sr`/`ss`/`et`/`cn`; the AP2 form drops those and adds
   `ft`/`pk`/`ov`. That record gets switched, not extended.

### But the ladder forks — mirroring is not on the AirPlay 2 branch

This is the part worth knowing before planning around it:

```
                     AP1 audio, et=0          ← where we are
                          │
          ┌───────────────┴───────────────┐
          ▼                               ▼
   AP1 audio, et=1                 AP2 audio
   (RSA unwrap; needs the          (HomeKit transient pairing + ChaCha
    leaked AirPort private          control channel + SETUP plist + PTP)
    key — §5.3 problem 3)          no FairPlay, no licence question
          │                               │
          ▼                               ▼
   AP1 mirroring                   AP2 mirroring
   (FairPlay / OmgHax)             ✗ nobody open-source has ever done this
```

Mirroring in the open world is **exclusively** the AirPlay 1 legacy path — UxPlay and
RPiPlay both, and both need the OmgHax unwrap. shairport-sync and airplay2-receiver are
audio-only by design. So "get to AirPlay 2" and "get mirroring" are different directions
from here, not successive rungs, and AP2 does not bring mirroring closer.

### What AirPlay 2 would cost

- HomeKit transient pairing: ~800–1000 lines, but **every constant is known** (§4) and
  pyatv's client drives it, so it is verifiable in CI with no hardware.
- The ChaCha20-Poly1305 framed control channel: small, and the seam already exists in
  `substrate-rtsp`'s `ByteTransform`.
- The two-phase `SETUP` plist and stream types 96/103.
- **PTP — the long pole.** A full IEEE-1588 slave. shairport-sync does not implement it
  at all; it shells out to a separate daemon (`nqptp`) that needs *exclusive* UDP 319 and
  320, which is also why shairport-sync cannot run in AP2 mode on macOS. This wants its
  own `substrate-ptp` crate on its own schedule, and it should not block anything else.

The UxPlay-shaped alternative — AP2 with `timingProtocol: NTP` instead of PTP — trades
PTP for FairPlay, so it is not actually cheaper.

## 3. The FairPlay question (#39) is largely retired

Q1 (now issue #39) said FairPlay-SAP "needs real captures from `airplay2-receiver`/UxPlay
against a live iOS sender." Three findings each independently reduce that.

**3.1 "The ~568 bytes" is not a message size.** It is exactly `4 × 142` — the four canned
`/fp-setup` SETUP1 replies. They are byte-identical across UxPlay
(`lib/fairplay_playfair.c:22-25`), pyatv (`server_auth.py:97-102`), airplay2-receiver
(`ap2/playfair.py:118-122`), shairport-sync (`rtsp.c:1858-1893`) and RPiPlay — **diffed, not
assumed.** They are 568 bytes of literal. Confidence: HIGH.

**3.2 Both `/fp-setup` round trips are trivially implementable.**

```
SETUP1: request 16 bytes  → response 142 bytes = reply_table[body[14]]
SETUP2: request 164 bytes → response 32 bytes  = fp_header[12] ‖ body[144..164]
```

SETUP2 is a header and a 20-byte echo. **There is no crypto in it at all.** The 164-byte
request must be retained for the whole session — it is an input to key derivation.
(`uxplay/lib/fairplay_playfair.c:48-80`.) Confidence: HIGH.

**3.3 The derivation step has published test vectors.** `airplay2-receiver`'s `fp_decrypt.py`
ships **20 complete vectors**: `(keyMsg 164B hex, encrypted key 72B base64, expected key 16B
hex)`, covering all four modes plus 16 extras. A from-scratch Rust port can be validated with
**zero hardware**. Confidence: HIGH.

### What is still genuinely blocked

- **Which flow our specific advertisement provokes from *current* iOS.** The bit→behaviour
  evidence (§4.2) is one author's empirical log from ~2021–22 and the bit *names* are
  disputed. A tcpdump of the first four requests from one real iPhone settles it in thirty
  seconds and is worth more than further reading.
- **Whether mirroring works over the AP2 transient path.** No open-source project has done
  it, so there is no prior art to copy and no fixture to check in.
- **The exact SETUP plist for mirroring on current iOS** (`streamConnectionID`, `latency*`,
  `supportedFormats`).
- Apple-DRM'd content. Not a gap — a boundary. Say so in `STATUS.md`.

### What `crypto-fairplay` gets wrong today

The crate is modelled one level too coarsely. `setup1`/`setup2` both return `NotImplemented`
when both are implementable; the boundary belongs on a third method,
`decrypt_ekey(&[u8; 72]) -> Result<[u8; 16], _>`. Two parser bugs:

- The **mode byte is at offset 14**, not `body[4]`. Offsets 5 and 6 are `type` and `seq`.
- `Stage` should be derived from the sender's `seq` field (1 vs 3), not an internal counter.
  The sender is authoritative about which message it is sending.

---

## 4. The advertisement

### 4.1 Encoding

`features` is 64-bit. In TXT it is `0x<low32>,0x<high32>` — **low word first**, which is the
opposite of how you would write the number, and the most commonly inverted detail in this
stack. In `/info` it is the same value as a single integer.

Confirmed arithmetically against a real capture: Apple TV 4K advertises
`features=0x4A7FFFF7,0x4155FDE` in TXT and `294246758999916535` in `/info`;
`(0x4155FDE << 32) | 0x4A7FFFF7` is exactly that. Confidence: HIGH.

### 4.2 Two published bit tables disagree

| Bit | openairplay `features.md` | pyatv / owntone / airplay2-receiver / emanuelecozzi |
|---|---|---|
| 26 | `HasUnifiedAdvertiserInfo` | `Authentication_8` (MFi) |
| 30 | `RAOP` | `HasUnifiedAdvertiserInfo` |
| 38 | `SupportsCoreUtilsPairingAndEncryption` | `SupportsUnifiedMediaControl` |
| 48 | `SupportsTransientPairing` | `SupportsCoreUtilsPairingAndEncryption` |

Use the right-hand column — four independent implementations agree on it, and pyatv's own
comment flags the openairplay table as suspect (`utils.py:50-54`: *"seems to be some
inconsistencies"*). Confidence HIGH that they disagree; MEDIUM that the right column is
right — nobody has Apple's header.

**Consequence for ground rule 1:** encode bit *numbers* with doc comments naming the disputed
candidates. Do not encode a semantic name for 26/30/38/48 in a Rust type.

### 4.3 Bits that are load-bearing

| Bit | Effect if wrong |
|---|---|
| **9** `SupportsAirPlayAudio` | Clear → owntone-class senders **drop the device from the list entirely** (`airplay.c:4046-4051`). Class A failure. |
| **7** `SupportsAirPlayScreen` | Clear → no sender ever offers a screen. Our current mask has it **clear** while the comment above it claims mirroring. |
| **27** `SupportsLegacyPairing` | Changes the **media key derivation**, not just whether pairing happens: on → `aeskey = SHA512(aeskey ‖ ecdh_secret)[0..16]`; off → no hash. Mismatched, the session completes cleanly and then renders noise. |
| 14 / 12 / 2 | Sender runs `/fp-setup`. |
| 26 | Sender expects `/auth-setup` with a real MFi coprocessor. shairport-sync **removed** this bit in 4.3 for exactly this reason. |
| 40 | Buffered-stream SETUP (type 103) + `POST /command`. Gated on `srcvers >= 354.54.6`. |
| 41 (+47) | **PTP on UDP 319/320.** Gated on `srcvers >= 366`. |
| 46 / 48 / 43 | HomeKit pair-setup. We answer `501`. |
| 51 | *"iOS just fails at Pair-Setup [2/5]"* — nobody open-source has made it work. |

Also: `acl=1` makes pyatv mark the device **unpairable** (`utils.py:266-269`). Emit `acl=0` or
omit. `sf`/`flags` bit 3 (`0x8`) or bit 9 (`0x200`) make pairing Mandatory.

### 4.4 Known-good masks, decoded from real captures

| Device / impl | `features=` | Character |
|---|---|---|
| **UxPlay 1.65+ (mirroring, no pairing)** | `0x527FFEE6,0x0` | Entirely low-word — no AP2 bits, so senders take the AP1 legacy path. **The model for Path B.** |
| **shairport-sync 5.x (AP2 audio)** | `0x405C4A00,0x18340` | Deliberately no 15/16/17 (metadata), no 26 (MFi), no 46, no 27. **The model for Path C.** |
| Libratone Loop (real, AP1-only) | `0x444C0A00` | Bits 9,11,18,19,22,26,30. **The smallest working audio mask observed.** |
| Marantz NR1607 (real, AP1-only) | `0x444F8A00` | Single word, no AP2 bits, no `pk`, no pairing — and it works. |
| **Denon AVR-X3500H / Sonos Symfonisk (real)** | `0x445F8A00,0x1C340` | ← **our current value.** |

### 4.5 Where our current value came from

`advert.rs:36`'s `features = "0x445F8A00,0x1C340"` is not invented. It is byte-for-byte the
**Denon AVR-X3500H / Sonos Symfonisk audio-only speaker mask**, from the openairplay spec's
worked example. It has **no mirroring bit**, and it promises HomeKit pairing (46), transient
pairing (48), unified media control (38), MFi auth (26), buffered audio (40) and PTP (41+47)
— six things we answer `501` to or cannot serve.

That single line explains both halves of the current failure: nothing will ever offer us a
screen, and anything that offers us audio hangs at pair-setup.

---

## 5. The media planes

### 5.1 AP1 / RAOP audio

Needs neither FairPlay nor pairing. The key arrives in the **ANNOUNCE SDP** as
`a=rsaaeskey:` (RSA-OAEP under the leaked AirPort Express key, advertised `et=1`) or absent
entirely (`et=0`), with `a=aesiv:`.

Encryption presence is a **three-way, not a boolean** (`shairport rtsp.c:3550-3560`): both
attributes absent → unencrypted; both present → encrypted; exactly one → `456 Header Field
Not Valid for Resource`. A natural `StreamCrypto::{None, Aes{key,iv}}` built at parse time.

The AES-CBC rule everyone gets wrong (`shairport player.c:1566-1582`, `uxplay
raop_buffer.c:117-140`, byte-identical):

- Only `floor(payload_len/16)*16` bytes are encrypted. The trailing `len % 16` bytes are
  **plaintext and copied verbatim**. No padding, no PKCS#7.
- **The IV is re-initialised from `a=aesiv` for every packet.** CBC state does not chain
  across packets.
- The 12-byte RTP header is not part of the CBC input.

Payload types — note that **retransmit replies arrive on either socket**, which UxPlay does
not handle and shairport does:

| PT | Socket | Direction | Meaning |
|---|---|---|---|
| 96 | audio | in | ALAC/PCM audio |
| 86 | audio **and** control | in | retransmit reply — strip 4 bytes, then a complete PT-96 packet |
| 85 | control | **out** | resend request (8 bytes) |
| 84 | control | in | sync/anchor (20 bytes) |
| 82 / 83 | timing | **out** / in | NTP request / reply (32 bytes each) |

**Minimum viable clock:** skip the entire NTP exchange. Run a fixed ~11025-frame FIFO;
underrun → insert silence, overrun → drop oldest. Answer `RECORD` with
`Audio-Latency: 11025` and be honest about it. This drifts by tens of ppm — one inserted or
dropped packet every few minutes, audible as a tick on sustained tones. Acceptable for first
light; the sync-packet handler (PT 84, anchored to local arrival time) is the cheap step 2.

**Decoders:** ffmpeg covers ALAC, AAC-LC, AAC-ELD (since 2013) and Opus. No new dependency.
**But `pipeline`'s `AudioDecoder::new` does not set `extradata`, and libavcodec's ALAC
decoder hard-refuses to open without a 36-byte magic cookie** built from the `fmtp` integers.
That is the one required `pipeline` change, and AAC-ELD needs the same seam (its config is the
4-byte constant `f8 e8 50 00`).

### 5.2 Legacy mirroring

Setup is RTSP on 7000: `/fp-setup` ×2, then `SETUP` #1 carrying `ekey`(72B)/`eiv`(16B) →
reply `timingPort`/`eventPort`, then `SETUP` #2 carrying `streams:[{type:110,
streamConnectionID}]` → reply `streams:[{type:110, dataPort}]`. The sender then opens a plain
TCP connection to `dataPort`. There is no `RECORD` gate on the mirror plane.

**The data channel is not RTP.** `substrate-rtp` and `ReorderBuffer` do not apply. Framing is
a fixed 128-byte little-endian header + payload:

| off | field | notes |
|---|---|---|
| 0 | `payload_size` u32 LE | |
| 4 | `payload_type` u8 | 0 video, 1 codec config, 2 heartbeat, 5 stats |
| 6 | flags u16 LE | bit `0x40` = stream suspending (client sleeping) |
| 8 | NTP timestamp u64 **LE** | **no epoch offset** — device uptime |
| 56 / 60 | encoded width / height f32 LE | ← use these for the decoder |
| 16 / 20 | source width / height f32 LE | ← use these for aspect |

Cross-validated between UxPlay's 2019+ code and an independent 2012 iPad hexdump in
`openairplay/airplay-spec`. Confidence: HIGH.

Type 0 payloads are **AVCC** — `[u32 BE length][NAL]` repeated. Rewriting each length with
`00 00 00 01` is **length-preserving and allocation-free**. Type 1 is a bare
`AVCDecoderConfigurationRecord`; its SPS/PPS must be prepended to the *next* type-0 packet,
which carries an identical timestamp.

**Crypto — the detail most likely to be got wrong.** AES-128-**CTR** (not CBC; CBC is audio),
and **the keystream is continuous across the entire connection**, not per-frame. UxPlay's
`aes_ctr_start_fresh_block()` dance looks like a reset but traces to the opposite: one
uninterrupted keystream over the concatenation of every type-0 payload in arrival order.

- Hold **one** `Ctr128BE<Aes128>` for the connection. RustCrypto's `StreamCipher` tracks
  sub-block position, so UxPlay's whole `og`/carry-buffer mechanism disappears.
- Never call `apply_keystream` on type 1/2/5 payloads — they consume no keystream.
- **Never drop or reorder a type-0 payload before decryption.** Drop after depacketisation,
  at the `EncodedFrame` level, where `render_pipeline.rs:766` already drops on a full channel.
- This is the **opposite** of Cast's per-frame nonce (`proto-cast/src/mirror.rs:291`), which
  is loss-tolerant by design. Don't let the in-repo precedent mislead the implementation.

Key derivation, agreed by UxPlay, RPiPlay and a decompiled Android receiver:

```
aeskey = fairplay_decrypt(keymsg_164, ekey_72)                    -> 16 bytes
if legacy pairing happened: aeskey = SHA512(aeskey ‖ ecdh_secret)[0..16]
key = SHA512("AirPlayStreamKey" + decimal_u64(streamConnectionID) ‖ aeskey)[0..16]
iv  = SHA512("AirPlayStreamIV"  + decimal_u64(streamConnectionID) ‖ aeskey)[0..16]
```

No separator, no null byte — `strlen` of an `snprintf`. **`streamConnectionID` is unsigned
u64 arriving in a signed plist field**; formatting it signed yields a `-…` string for ids
≥ 2⁶³, a different SHA input, and the classic symptom *"correct image for a while, then
garbage."* A `u64` newtype whose `Display` is unsigned makes that a compile-time
impossibility.

**Pipeline changes required: none.** `decode_stream` already wants Annex-B with in-band
SPS/PPS, has no clock, and drops late frames. `EncodedFrame.pts` is duration-since-start,
computable from the mirror header alone — the entire NTP subsystem is dead weight for
video-only mirroring. It earns its keep only when mirror audio (AAC-ELD, type 96) needs
lip-sync.

**The event channel can be ignored.** UxPlay hard-codes `eventPort: 0` with the comment *"the
event port is not used in mirror mode or audio mode"* and mirrors fine from iOS 12 through
iOS 18. Return 0 and implement nothing.

**Geometry is not negotiated.** Advertise a height budget; iOS picks an encode geometry that
fits and reports it in the type-1 header floats, re-sending type-1 on every rotation.
Advertise 1920×1080 with bit 42 clear for the first cut — with bit 42 set the sender may send
HEVC, and the failure mode is a type-1 packet with `payload_size == 0`.

### 5.3 What FairPlay actually costs

Two distinct things, and #39 conflated them:

- **The 4 × 142 canned replies** — 568 bytes, published since ~2012, byte-identical
  everywhere. Ship them.
- **The OmgHax key-derivation tables** — ~99 KiB (computed exactly from `omg_hax.h`) plus
  ~1200 lines of algorithm: a bespoke scrambler, a modified MD5, an AES-like round function,
  and `hand_garble.c`, whose own author wrote *"I have no idea what this is doing (yet), but
  it gives the right output."* A dependency-free pure-Python port exists to transcribe from.

**The licence problem is two problems.** The Apple one: UxPlay's own README declines to defend
the tables — *"The legal status of that library is unclear."* And an independent one: the C
and the Python are both **GPL**, and castaway is MIT. A clean-room port from the algorithm
description plus the constants is the usual answer. This is a decision to make deliberately,
not to arrive at by writing code. Path C avoids it entirely.

### 5.4 AirPlay Video / HLS handoff — assessed and deprioritised

The naive read ("the pipeline already plays URLs, so `/play` is nearly free") does not survive
contact with the tree:

- **`rtsp-types 0.1.3` cannot parse `HTTP/1.1`** — `parser.rs:49` hard-codes
  `tag("RTSP/")`. Every AirPlay Video request arrives as `HTTP/1.1` **on the same port-7000
  socket**, so this path needs a second parser behind a protocol-token sniff, plus PTTH
  reverse-direction framing with in-band response sniffing.
- 11 endpoints, several stateful, versus mirroring's one.
- The moment a user tries YouTube — the single most likely thing they will try — they hit
  **FCUP**: the playlist is not network-reachable, it lives on the phone and must be fetched
  back over the reverse channel, rewritten, and re-served locally. UxPlay took two releases
  and 981 lines.

Its cheap slice (`/play` with a reachable URL, `/rate`, `/scrub`, `/stop`, `/playback-info`)
is real and worth having *later* — and becomes cheaper once the actor already distinguishes
protocols on 7000.

---

## 6. Testing without Apple hardware

### 6.1 Senders, ranked

| Rank | Tool | nixpkgs | Covers |
|---|---|---|---|
| 1 | **`cliraop`** | `libraop` | Full AP1 session. Takes IP+port directly — **no mDNS dependency**, so a discovery failure cannot masquerade as a protocol failure. Reads stdin (`ffmpeg -f lavfi -i sine=…` → a deterministic tone). `-cmdpipe` for mid-session volume/metadata. `-if` to pin the interface. |
| 2 | **`atvremote`** | `python313Packages.pyatv` | Same, **plus real mDNS discovery** (tests our TXT as something a client parses, not something avahi echoed back), **plus the full AP2 transient-pairing path** — `/pair-pin-start`, TLV8 M1–M4, SRP-6a with PIN 3939, ChaCha20 event channel. Pure Python in the store, so trivially instrumented. |
| 3 | `cliairplay` | — (package it) | The only C implementation of native AP2; a second independent oracle. |
| 4 | **`doubletake`** | — (package it) | Go; claims full AP2 mirroring + FairPlay SAP + SRP-6a, with a daemon mode over a Unix socket. **The only credible route to autonomous mirroring coverage.** |
| 5 | PipeWire `module-raop-sink` | `pipewire` | The mainstream-stack test — a more pedantic TXT reader than pyatv. |

**Rejected:** VLC has **no RAOP sender at all** (verified by listing
`lib/vlc/plugins/stream_out/`); VLC 3's renderer support is Chromecast-only. GStreamer has no
AirPlay sink in nixpkgs. `shairplay` is a server, unmaintained since 2018.

### 6.2 Two checks that need no capture at all

- **`checks.pyatv-fairplay-tables`** — read the four 142-byte SETUP1 replies out of the
  pinned pyatv derivation (`server_auth.py:98-125`) and `cmp` against checked-in fixtures.
  Exactly the shape of the existing `nix/openscreen-fixtures.nix`. Closes the stage-1/stage-2
  half of §3 and pins it against upstream drift.
- **A features round-trip table** — for each mask in §4.4, `(txt_string, u64, decoded bits)`.
  Makes the low-word-first ordering unrepresentable-if-wrong and catches the `/info`
  truncation (D6 below) as a test rather than a silent mismatch.

### 6.3 Capture mechanics

**Use a tee proxy, not pcap.** ~50 lines of Python listening on 7000/7011 and dialling
through, appending length+direction+timestamp-framed chunks. Per-direction, already
reassembled, works inside `nix build` sandboxes, no TCP reassembly and no `decode-as`
guessing. tshark cannot decrypt AP2 and there is no session-key log format for it.

**Determinism despite crypto:** pyatv is Python in the store, so a ~15-line `sitecustomize.py`
monkeypatching `Chacha20Cipher.__init__` to append keys to `$CASTAWAY_KEYLOG` is the AirPlay
analogue of `SSLKEYLOGFILE`. Pin the sender's ephemeral X25519 private key into the fixture
directory so the pure test can reconstruct the sender deterministically. Receiver key
generation must be injectable (`new_with_rng`) for the same reason `device_auth_vectors.rs`
pins `AT` and uses an RSA fixture key.

**When an iPhone is eventually in the room, the tool is `atvproxy`, not tcpdump** — it stands
up a fake receiver, terminates the phone's pairing itself, and logs *decrypted* RTSP at DEBUG,
converting "encrypted pcap you can never read" into a plaintext transcript. Do a **Mac**
capture first; `tcpdump -i en0` on the sending Mac needs no jailbreak, where an iPhone needs
`rvictl -s <UDID>`.

**nixosTest gotchas:** every node has NAT `eth0` plus the test VLAN `eth1`, so unbound
multicast egresses the wrong way. `cliraop -if <lan>` and pyatv's local-IP binding are both
mandatory — get them wrong and RTSP succeeds while audio and NTP vanish, which looks exactly
like a broken receive path. The sender node needs its firewall off, because it opens
*inbound* UDP for the control and timing channels.

---

## 7. Two build orders

### Path A — sound from an iPhone, fewest steps

0. **Fix the advertisement** (§8). `_raop._tcp` only, `et=0` (or `0,1` with the RSA unwrap),
   `cn` matching the decoder, no AP2 bits. Half a day, unblocks everything.
1. RSA key unwrap + `Apple-Challenge` signing — pure, one fixture.
2. SDP session-description parser — pure. **Not in `substrate-sdp`, which is Bluetooth
   Service Discovery Protocol.** A `proto-airplay/src/sdp.rs` or a new crate.
3. Grow `AirPlaySession` a real `Announced → Setup → Recording` typestate. Make `SETUP`
   before `ANNOUNCE` unrepresentable (shairport answers 451).
4. The audio depacketiser — pure `fn(state, datagram) -> (state, outputs)`.
5. `pipeline` extradata seam (§5.1).
6. The socket actor: three UDP sockets bound *before* answering SETUP.
7. Control surface — DMAP metadata, volume (dBFS, -144 = mute, usable range -30…0), progress.

### Path B — the headline gesture

0. **Fix the advertisement** to UxPlay's `0x527FFEE6,0x0`, fix `/info`'s `displays[0]`, and
   make `SETUP` a real handler returning a `dataPort`. **You can watch a real iPhone complete
   the entire handshake and open a TCP connection before writing a line of crypto** — the
   cheapest possible reality check.
1. The pure mirror-stream core — header parse, AVCC→Annex-B, SPS/PPS prepend, suspend/resume.
   Golden fixtures from the openairplay hexdumps are checked in and free.
2. FairPlay: stages 1 and 2 (a table and a memcpy), then `decrypt_ekey` against the 20
   published vectors, then the SHA-512 stream-key derivation.
3. The actor: TCP listener on `dataPort` → `EncodedFrame`s → `SessionEvent::Mirror`.

Both paths start with the same half-day of advertisement work, which is the argument for
doing that first regardless of which follows.

---

## 8. Defect list

Line references are to the tree at the time of writing.

### Fatal — a sender that finds us cannot complete a session

| # | Where | Now | Should be |
|---|---|---|---|
| D1 | `advert.rs:36` | `features "0x445F8A00,0x1C340"` | Depends on the §2.1 fork. It is the Denon/Sonos audio-speaker mask, promising bits 26/38/40/41/46/47/48 we answer `501` to. |
| D2 | `advert.rs:36` | bit 7 clear, comment claims mirroring | Set bit 7 **only** when mirroring works. The comment and the constant describe different devices. |
| D3 | `advert.rs:40` | `pk=""` | **Omit the key.** No real device advertises an empty `pk`; pyatv uses it as a fallback device identity and pair-verify has nothing to check against. Marantz and Libratone omit it and work. `Option<Ed25519PublicKey>`, never a `String`. |
| D4 | `advert.rs:10`, `actor.rs:76` | `RAOP_PORT = 7011` | **7000** (shared, as shairport-sync and UxPlay both do) or **5000** for a separate AP1 RAOP listener. 7011 is the AirPlay-1 **UDP NTP timing** port. |
| D5 | `advert.rs:54` | `et = "0,3,5"` | `"0"` today. `et=3,5` promises FairPlay; `/fp-setup` returns 501. shairport-sync explicitly *removed* `et=4` in 4.3 rather than advertise auth it could not perform. |

### Serious — wrong on the wire, silently

| # | Where | Now | Should be |
|---|---|---|---|
| D6 | `info.rs:21` | `features` = low word only | The **full 64-bit** value. `/info` currently claims a smaller feature set than the TXT — a sender reading both sees a contradiction. |
| D7 | `info.rs:15` | `"deviceid"` | **`"deviceID"`.** `/info` uses different capitalisation from TXT for the same data: also `sourceVersion`, `protocolVersion`, `statusFlags`. Three independent real captures agree. |
| D8 | `info.rs:28-29` | `refreshRate: Integer(60)` | `Real(1.0/60.0)`. It is a **frame period**, not a rate. `60` reads as a 60-second period. |
| D9 | `info.rs:26-29` | `width`, `height` | `widthPixels`/`heightPixels`, plus the missing `uuid`, `widthPhysical`, `heightPhysical`, `rotation`, `maxFPS`, `overscanned`, `features`. A `displays[0]` without `uuid` is not a display the sender can address. |
| D10 | `advert.rs:41`, `info.rs:23` | `pi` = MAC string | A stable **UUID**. Only Roku and Samsung put a MAC there. |
| D11 | `advert.rs:47-62` | `_raop._tcp` has no `ft`/`sf`/`pk`/`pw` | Add them, derived from the same variables as `_airplay._tcp`. **pyatv reads `ft` before `features`** — on a RAOP service `ft` is the only capability signal. |
| D12 | `advert.rs:52` | `cn = "0,1,2,3"` | Whatever the decoder handles. Promises AAC-ELD; shairport-sync, a mature audio receiver, advertises `cn=0,1`. |
| D13 | `app/src/main.rs:997` | `derive_mac` takes UUID hex verbatim | Force locally-administered unicast: `b[0] = (b[0] & 0xFE) \| 0x02`. ~50% of UUIDs set the **multicast bit**, which is not a legal MAC. |
| D14 | `session.rs` | `TEARDOWN` has no `Connection: close` | Add it; shairport closes the socket after. |
| D15 | `session.rs` | `SETUP` before `ANNOUNCE` → lenient `200` | `451`. |
| D16 | `substrate-mdns/src/service.rs:80` | TXT into a `HashMap` | Order randomises per process, defeating byte-exact `txtAirPlay` fixtures (ground rule 6). Keep the `Vec` ordering through to `ServiceInfo`. |

### Correct — do not "fix" these

- **`flags = "0x4"`** — status-flag bit 2, "audio cable attached". Every idle receiver in
  every capture advertises exactly this. It looks like a placeholder and is right.
- **RAOP instance `AABBCCDDEEFF@Name`**, uppercase, no separators. pyatv splits on the first
  `@` for device identity.
- `vn = 65537` (0x00010001 = v1.1), `txtvers = 1`, `ch = 2`, `sr = 44100`, `ss = 16`,
  `md = 0,1,2`, `da = true`.
- `/info` as a **binary** plist. The XML form belongs to the *different* legacy
  `GET /server-info` endpoint.
- Echoing `CSeq`; advertising the port actually bound rather than the constant.

---

## 9. Unresolved disagreements

Recorded rather than resolved.

- ~~**UxPlay's current bit-27 default.**~~ **Settled by reading the tree.** At
  `acfb549` (2026-07-20), `lib/dnssdint.h:32-33` ships `FEATURES_1 "0x5A7FFEE6"` — bit 27
  **ON** — with `0x527FFEE6` present but commented out, and `uxplay.cpp:1984` documents
  the default as `0x5A7FFEE6`. So the pairing-bypass is a real, README-documented option
  that UxPlay does *not* take by default. Anyone reaching for it should note that bit 27
  also changes the media key derivation (§4.3), and that the off-path is therefore less
  continuously exercised than the on-path.
- **Feature-bit names for 26/30/38/48** (§4.2). Positions are evidence; names are folklore.
- **iOS 18+ behaviour.** shairport-sync issue #1866 (iOS 18 beta connect failure) was closed
  "not planned" with no published root cause. Community reports of iOS 26 degrading mirroring
  to audio-only are LOW confidence with no receiver-side fix published.
- **Whether `pk`-less, pairing-less receivers stay listed** in future iOS. Marantz- and
  Libratone-class devices are old and nothing guarantees it.

## 10. What is fragile and needs a "revisit per major iOS" note

1. **`srcvers` is a behaviour switch, not a version string.** `>= 354.54.6` turns on buffered
   audio in senders; `>= 366` turns on PTP. Our `377.40.00` is above both gates while we
   implement neither. Path A wants UxPlay's `220.68`; Path C wants shairport's `366.0`.
   Whatever is chosen should carry a comment saying what it switches on.
2. Legacy pairing / the whole AP1 mirroring path. Apple has never promised it. UxPlay
   README: *"there is no guarantee that future iOS releases will keep supporting 'Legacy
   Protocol', iOS 17 continues support."*
3. Bits 40/41 vs 45 — buffered/PTP vs realtime/NTP. The sender's choice is `srcvers`-gated,
   so a version bump silently changes the media plane.
4. CVE-2025-60458: a crafted `TEARDOWN` crashed UxPlay. Worth noting as precedent for our own
   `TEARDOWN` path.
