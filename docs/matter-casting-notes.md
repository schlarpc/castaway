# Matter Casting protocol notes

The record `proto-matter` is built from. Read before changing it. §6 is where the
remaining risk lives.

The decision to link `rs-matter` rather than reimplement, and what it cost, is
**DECISION-LOG D54** — not repeated here.

Sources, in order of how much they are worth:

1. `project-chip/connectedhomeip`, `src/protocols/user_directed_commissioning/` — the
   implementation *is* the specification for UDC. Its own header carries a `// TODO:
   update spec per the latest tags` comment against the tag numbers, so where the spec
   text and the code disagree, the code is what phones speak.
2. The CSA **Matter Core Specification** (commissioning, PASE/CASE, DNS-SD) and
   **Application Cluster Specification** (the media clusters, the Video Player and Content
   App device types). Both public, both freely downloadable. "Matter Casting" is not a
   separate document — it is those two, plus a marketing name.
3. `project-chip/connectedhomeip`, `examples/tv-app` and `examples/tv-casting-app` — the
   reference receiver and sender. The receiver ships dummy content apps that do not play
   anything, so it proves the control path and nothing else.
4. `docs/casting-dossiers.json`, the "Matter Casting" entry — this project's own research
   record, and the source of the framing in §1.

---

## 1. The roles are inverted

This is the single most important fact about the protocol and the one most often stated
backwards.

| | Ordinary Matter | Matter Casting |
|---|---|---|
| Commissioner | the phone / hub | **the TV** |
| Commissionee | the device | **the phone** |
| Who ends up serving clusters | the device | **the TV** |

The Casting Client (phone app) is commissioned *onto a fabric the Casting Video Player
administers*. Once it has an operational certificate it opens a CASE session **back** to
the player and invokes the media clusters on it.

Two consequences that shape the whole crate:

- The panel needs both directions of PASE and CASE, an interaction-model **client** during
  commissioning and an interaction-model **server** afterwards, and a certificate
  authority. That is why `crates/proto-matter/src/fabric.rs` exists at all.
- Both roles ride one UDP socket (5540). The commissioning worker and the responder are
  concurrent futures over the same transport.

**No media crosses this protocol.** `LaunchURL` is a sentence. The named app fetches its
own bytes from its own backend, which is why a receiver is only as useful as the content
apps it can honestly host (§5).

---

## 2. User Directed Commissioning

`rs-matter` does not implement UDC. `crates/proto-matter/src/udc.rs` does, from source 1.

### 2.1 Framing

UDP **5550**, and the only Matter exchange with no session, no encryption and no
acknowledgement. The client sends **five identical datagrams, 100 ms apart**, because
there is nothing to retransmit against.

Fourteen fixed header bytes, and they cannot be anything else — nothing has an operational
identity yet, so every optional field of the general message format is absent by
construction:

```
00                 message flags: version 0, no source node id, DSIZ 0
00 00              session id 0
00                 security flags: unicast, unencrypted
00 00 00 00        message counter — 0, which is what the reference sends
01                 exchange flags: initiator, no ack requested
00                 opcode
00 00              exchange id — 0; the reference never sets it
09 00              protocol id 0x0009, User Directed Commissioning
```

Both directions use **opcode 0x00**. There is no message-type byte distinguishing them;
the direction does. The reference server's `SendCDCMessage` writes
`MsgType::IdentificationDeclaration` into the `CommissionerDeclaration` it sends, which
looks like a bug and is not — it is the wire.

Message counter 0 on every copy is likewise deliberate upstream: `SetMessageCounter` is
never called for UDC, so a peer that deduplicated on the counter would drop four of the
five copies of a message that has no acknowledgement.

### 2.2 `IdentificationDeclaration` (client → player)

The payload is **not** pure TLV. It opens with a fixed-length NUL-padded block:

```
+---------------------------+---------------------+
| instance name, NUL-padded | TLV structure       |
+---------------------------+---------------------+
|<------ 17 bytes --------->|
```

`char mInstanceName[kInstanceNameMaxLength + 1]` — 16 characters plus the terminator, and
the TLV starts at offset 17 *whatever the name's real length*. Reading it as
NUL-terminated-and-packed puts the TLV parser in the padding.

A payload that is exactly the name block is legal and means "commission me, I have nothing
else to say"; the reference reader returns early on it.

TLV context tags, from `IdentificationDeclarationTLVTag`:

| Tag | Field | Notes |
|---|---|---|
| 1 | `vendorId` | u16 |
| 2 | `productId` | u16 |
| 3 | `deviceName` | ≤32 chars — the phone's name, for the prompt |
| 4 | `deviceType` | in the enum; **neither written nor read** by the reference |
| 5 | `pairingInstruction` | ≤128 chars |
| 6 | `pairingHint` | u16 bitmap |
| 7 | `rotatingId` | ≤50 bytes; the Account Login passcode flow uses it |
| 8 | `cdPort` | where to send the reply. **0 means "do not"** — it is the field's default |
| 9 | `targetAppList` | a TLV *list* |
| 10 | `targetApp` | a struct inside it |
| 11 | app `vendorId` | vendor 0 entries are dropped, not matched |
| 12 | app `productId` | 0 is a wildcard over the vendor |
| 13 | `noPasscode` | |
| 14 | `cdUponPasscodeDialog` | |
| 15 | `commissionerPasscode` | "you generate it and show it" — §3 |
| 16 | `commissionerPasscodeReady` | "the user has typed it" |
| 17 | `cancelPasscode` | |
| 18 | `passcodeLength` | u8 |

Tags 13/16/17 are three booleans that are really one three-way choice; the reference
server tests them in the order cancel → ready → fresh and dispatches to three unrelated
handlers. `UdcRequest` resolves that once, at parse time.

### 2.3 `CommissionerDeclaration` (player → client)

Pure TLV — **no** instance-name block. Same framing bytes.

| Tag | Field |
|---|---|
| 1 | `errorCode` (`CdError`, 0–18) |
| 2 | `needsPasscode` |
| 3 | `noAppsFound` |
| 4 | `passcodeDialogDisplayed` |
| 5 | `commissionerPasscode` |
| 6 | `qrCodeDisplayed` |
| 7 | `cancelPasscode` |
| 8 | `passcodeLength` |

Every field is written unconditionally, including the false and the zero. We do the same:
a client reading by position rather than by tag breaks on a sparse struct, and such
clients exist.

The reply goes to the **`cdPort` the client named**, at the source IP — not to the source
port. A client whose UDC sender and listener are different sockets is normal.

### 2.4 What a phone actually does, end to end

1. Browses `_matterd._udp`, finds the panel, reads `DT`/`VP` from the TXT record.
2. Sends five `IdentificationDeclaration`s with `commissionerPasscode` set.
3. Panel shows a passcode, answers with `passcodeDialogDisplayed`.
4. **A person walks to the phone and types it.**
5. Phone starts advertising `_matterc._udp` with a verifier derived from that passcode,
   and sends `commissionerPasscodeReady`.
6. Panel browses for that instance name, finds it, runs PASE against it with the passcode,
   then `ArmFailSafe` → … → `AddNOC`, then CASE and `CommissioningComplete`.
7. Phone opens CASE **to the panel** and starts invoking clusters.

Step 4 is why the commissionable-node timeout is a minute and the passcode window is three:
one is waiting on a person walking, the other on a number being readable in a shared room.

---

## 3. The passcode, and why it is ours

Two flows exist. The client-generated one needs the user to type a number *into the TV*,
which needs a keyboard the panel does not have. The commissioner-generated one — the flow
Amazon's senders use — has the player generate and display it.

The panel implements only the second and declines the first with
`CommissionerPasscodeNotSupported`, which is the spec's own word for it (D32).

Eight digits, avoiding the twelve values Core §5.1.7.1 forbids (the repdigits, `12345678`,
`87654321`, and the two reserved). Displayed as `1234-5678`, the grouping the manual
pairing code uses, because the number is being read across a room.

**A retransmit must return the same passcode.** Nothing in the five copies distinguishes a
retransmit from a second attempt, and generating one per datagram changes the number four
times while somebody reads it — and invalidates the one they have started typing. The
window is measured from when the number first went up, not from the last copy, or a client
that re-declares on a timer holds a passcode on the screen forever.

---

## 4. Discovery

| Record | Direction | Carries |
|---|---|---|
| `_matterd._udp` | we advertise | `VP=<vid>+<pid>`, `DT=35` (Casting Video Player), `DN`, `SII`, `SAI` |
| `_matterc._udp` | we browse | the commissionable node named in a UDC message |
| `_matter._tcp` | **neither** | the panel's operational node lives only on a fabric it created; the phone learns that address while being commissioned |

`DT=35` is what makes a phone list the panel under "TVs" rather than as an unrecognised
node. It is decimal in the TXT record and `0x0023` everywhere else.

Instance-name matching between the UDC message and the mDNS label is **case-insensitive**.
The spec says uppercase hex; a phone that is consistently lowercase would otherwise be
undiscoverable, and from the panel that failure is indistinguishable from the user never
having typed the passcode.

The browse runs on its own `mdns-sd` daemon, like GameStream's — a third socket on 5353.

---

## 5. The endpoint tree

| Endpoint | Device type | Clusters |
|---|---|---|
| 0 | Root Node | `rs-matter`'s system clusters, untouched |
| 1 | Casting Video Player `0x0023` | Descriptor, ContentLauncher, MediaPlayback, TargetNavigator |
| 6… | Content App `0x0024` | Descriptor, ApplicationBasic, ContentLauncher |

Content apps start at endpoint 6 because the reference `tv-app` does and clients have been
seen assuming it.

`ApplicationBasic` is the **address** a client aims at, not a label: it picks an endpoint
by matching its own app's vendor and catalog id. `allowedVendorList` is deliberately empty
— on a certified receiver it is how a content app refuses a client whose vendor is not on
it, a decision made on the strength of the client's attestation certificate, and this panel
does not verify attestation. Empty means "no vendor is specially privileged", which is
true; a list would be a claim we cannot back.

`ContentLauncher` is advertised with both feature bits (URL playback and content search).
A `LaunchContent` search against an app with no search template is refused with a status,
not honoured by opening a home page and calling that a result.

`MediaPlayback` advertises AdvancedSeek only. Not variable speed: the panel's players do
not all have a rate control, and `Rewind`/`FastForward` therefore answer
`SpeedOutOfRange` rather than seeking and calling it rewind.

### 5.1 Conformance gaps

The Casting Video Player device type's full cluster conformance includes OnOff, WakeOnLan,
KeypadInput, ApplicationLauncher, AudioOutput and MediaInput. None is implemented. A
strict client may refuse an endpoint that does not present them; the reference casting
client does not. Tracked, not papered over.

---

## 6. Where the risk is

**Nothing has driven this from a real phone.** The wire tests script a Casting Client over
a real socket and prove the UDC exchange byte for byte; the commissioning half — PASE,
`AddNOC`, CASE — has never run against a peer. `rs-matter`'s own commissioning integration
test exercises that code path, but against `rs-matter`, which is agreement with ourselves.

**Attestation runs the *other* way, and the first draft of this section said otherwise.**
Worth stating precisely, because the inversion catches this too. During commissioning it
is the **client** that presents a Device Attestation Certificate and the **player** that
checks it — the player never presents one, and CASE afterwards authenticates with node
operational certificates chained to the fabric root the client was just given. So there is
no "receiver attestation chain" for a sender to validate, and nothing cryptographic on the
panel's side is what stands between it and a commercial sender.

We do not verify the client's, either: `allow_test_attestation: true`, and the comment in
`commission_one` says why — `rs-matter` has no DCL fetch, and what verification would prove
(that the phone is a certified Matter device) is not the question the panel is asking. The
question is whether the person holding it is in the room, and the passcode on the screen
answers that.

**What actually stands between this panel and a commercial sender is softer, and worse.**
A client picks an endpoint by matching `ApplicationBasic` — a vendor id, a product id, an
application id in some catalog. Those are numbers in config. Claiming Amazon's would be a
config edit, and it would be a lie the panel cannot make good on: the cast would be
accepted and then nothing would play, because the content app on the other side has to be
the real thing, with its own DRM stack and its own entitlement. Matter carries no media, so
there is no version of this where the protocol supplies what the app does not. That is a
ceiling, not a gap.

The vendor id itself is a registry entry — the CSA allocates them, `0xFFF1`–`0xFFF4` are the
test range, and the authoritative list is the CSA's distributed compliance ledger. The
SDK's own `CHIPVendorIdentifiers.hpp` names only a handful (Apple `0x1349`, Google `0x6006`,
the test range); every value in this repository's fixtures outside the test range should be
read as illustrative unless it cites the DCL.

**Content apps are the ceiling.** Matter gives the panel "app X, play title Y". Mainstream
apps will not run here — they need their own DRM stack and their own business arrangement —
so the panel's honest catalogue is itself plus whatever the browser can open. This is a
property of the protocol, not of the implementation.

**Audio-only casting does not exist.** The shipped architecture is video-centric; there are
no ratified audio-player endpoints and no multi-room sync in Matter. Marketing copy that
says "audio and video" is aspirational.
