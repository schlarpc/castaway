# DLNA/UPnP conformance — what was checked, and against what

A spec-grounded adversarial review of `proto-dlna` and the media path it feeds, done
2026-07-27 against the normative documents rather than from memory.

**GAPS.md holds the defects it found** (G68–G82). This file holds everything else, which
turned out to be the larger half: the things that are *correct* and must not be "fixed",
the claims the review made and then withdrew, how real control points behave where the
spec is silent, and what the citations here are actually worth.

That split matters. A defect list tells you what to do next. It does not stop the next
person from reading correct-looking-wrong code and helpfully breaking it, which is the
failure this file exists to prevent — twice during the review the obvious-looking fix was
the wrong one.

## Confirmed correct — do not "fix" these

Each of these looks like an omission or a mistake and is neither. The citation is why.

| Thing | Why it is right |
|---|---|
| `GetCurrentConnectionInfo` returning `RcsID=0`, `AVTransportID=0`, `PeerConnectionID=-1`, `Direction="Input"`, `Status="OK"`, and `CurrentConnectionIDs = "0"` | ConnectionManager:1 §2.2.3: *"If optional action PrepareForConnection is not implemented then this state variable should be set to '0'."* This is the prescribed default-connection shape, not a stub. |
| `PrepareForConnection` left unimplemented | CM:1 §5.1.1.2: *"This action is optional for the HTTP protocol."* Several control points use its **absence** to detect the DLNA default-connection model, so implementing it would change behaviour for the worse. The error code was the only thing wrong and is now 602 rather than 401 (G79). |
| `Previous` answering 701, and `Next` answering 701 unless something is staged | Both are *required* actions, so dropping them from the SCPD would be a conformance regression — but a renderer handed one URL has no playlist. `SetNextAVTransportURI` is the only queue it has, so `Next` advances into that and nothing else does. 701 "Transition not available" is the code §2.4.9 defines for exactly this, and it is a true statement rather than a stub. |
| `X_DLNADOC` saying `DMR-1.50`, not 1.51 | Rygel ships a `Dlna150Hacks` class that string-replaces `-1.51`→`-1.50` per-User-Agent because control points choke on 1.51. Do not "upgrade". Do not add `M-DMR` either — that is for devices advertising a reduced mandatory media-format set, which a fixed panel is not. |
| The namespace `urn:schemas-dlna-org:device-1-0` having **no** trailing slash | Distinct from the *metadata* namespace `urn:schemas-dlna-org:metadata-1-0/`, which does have one. They look like a typo of each other and are not. |
| Omitting `iconList` | UDA 1.1 §2.3 makes it REQUIRED *if and only if the device has icons*. We have none. |
| `state.rs::parse_upnp_time` and `didl.rs::parse_duration` being **separate parsers** | The grammars genuinely differ, and `TimeSeekRange` will need a *third* (RFC 2326 NPT, where a bare-seconds form like `npt=123.45-125` is legal). Merging them is the tempting wrong move. |
| DIDL parsed order-independently, matching on **local name** | The XSD requires `dc:title` to be the first child of `<item>`, and real senders violate that constantly. Leniency inbound is the correct posture; strictness costs a blank card for an item that told us its title. |
| `upnp:class` matched by substring rather than equality | Vendors legally extend the class taxonomy, so `object.item.audioItem.musicTrack.someVendorThing` must still read as audio. |
| Not requiring `dlna:profileID` on `albumArtURI` | It appears in **no** UPnP specification. minidlna emits it only on explicit request or for Samsung clients. Do not schema-validate inbound DIDL against the stock UPnP XSD either — it would reject documents that are fine in practice. |
| `config.uuid` being file-backed | UDA 1.1: the UDN *"MUST be the same over time for a specific device instance (i.e., MUST survive reboots)."* Worth not regressing into a generated-at-startup UUID. |

## Where real control points and the spec disagree

The spec does not describe what breaks. This does.

**Nobody depended on GENA except Home Assistant, and it depended on the absence being
honest.** This table is what made the case for refusing `SUBSCRIBE` outright — and it is
kept because it is still the reason eventing has to be *correct* rather than merely
present. Behaviour against a renderer that answers `SUBSCRIBE` with 200 and then never
sends an event:

| Control point | Behaviour |
|---|---|
| Home Assistant / `async_upnp_client` | **Worst case.** `is_subscribed` becomes true, which disables the polling fallback entirely — transport state, volume and mute freeze at connect values forever while the entity stays "available". A 200 *without* a `SID` is worse still: raises `UpnpSIDError`, not `UpnpResponseError`, so the entity goes permanently unavailable. |
| BubbleUPnP | Fine — ships with eventing off by default and polls. |
| foobar2000 `foo_upnp` | Fine — removed eventing in 0.99.26. |
| Kodi | Fine — subscribes, discards every event, polls at 2 Hz. |
| VLC, Symfonium, `dlnap` | Fine — poll. |
| Windows "Cast to device" | **Unproven.** WHCK EVENT-01 requires `LastChange` and justifies it as *"The controller implemented in Windows 8 relies on AVT and RCS LastChange events to make decisions about devices"*, but that it de-lists a silent renderer was not substantiated. Needs a real Windows box. |

That is why G68's *first* fix was to refuse with 501: it is better than accepting,
because it puts the one control point that cares back onto a path that works.

**Superseded — the renderer now events for real** (subscriber table, initial NOTIFY, `SEQ`,
renewals, `LastChange` for AVT and RCS). What the table above still tells you is where the
risk moved to. Home Assistant will now take our events and *not* poll, so a `LastChange`
that is wrong or missing is a frozen entity again by a different route — the failure this
table describes, reintroduced by getting the replacement wrong rather than by not having
one. Two specifics follow from that:

- **Publish on a diff, not on a mutation.** A control point polling `GetTransportInfo`
  twice a second must not produce two events, and no setter may be able to forget to raise
  one. Both fall out of computing what changed where the state is already in hand.
- **Keep position and duration out of `LastChange`.** §2.3.1 excludes position; duration
  follows it here because both are read from the pipeline per request rather than stored,
  so either one would make the diff differ on every poll — an event a second, per
  subscriber, for a number nobody asked to be pushed. Control points poll
  `GetPositionInfo` for these, which is what that action is for.

The Windows row stays **unproven** in the direction that matters: WHCK EVENT-01 is now
satisfied in shape, but whether Windows "Cast to device" is happy with our documents has
still never been observed on a real Windows box.

**Other places practice diverges from the text:**

- **MIME globs do not work.** gmrender-resurrect: *"BubbleUPnP does not seem to match
  generic `audio/*` types, but only matches mime-types _exactly_."* See G80.
- **`x-` prefixes are a coin toss.** The same source documents controllers disagreeing
  about `audio/x-m4a` vs `audio/m4a` vs `audio/mp4`. Emit both spellings.
- **`transferMode.dlna.org` is not mandatory**, and when sent its value comes from
  `DLNA.ORG_FLAGS` (`tm-i`→`Interactive`, else `tm-s`→`Streaming`, else omit) — *not* from
  the MIME type. Microsoft emits the header name as `TransferMode.DLNA.ORG:`, so match
  header names case-insensitively.
- **`DLNA.ORG_OP`'s first digit is TIME-seek and the second is BYTE-seek** — confirmed
  three ways (libdlna, anacrolix/dms, Rygel) and the most commonly inverted detail in the
  stack. It means *arbitrary* random access; *limited* seek is separate
  (`lop-npt`/`lop-bytes` in the flags).

  Seek has landed (G66) and did **not** need this, which is worth saying plainly so the
  next person does not go looking: we are a *renderer*, so `DLNA.ORG_OP` is something a
  server tells us, not something we publish. Our seek moves libavformat's demuxer, and
  whether that works is a property of the container and the transport rather than of a flag.
  Where the flags would earn their keep is the reverse direction — reading
  `contentFeatures.dlna.org` off the server's response to decide whether to *offer* seek for
  a given item, instead of offering it and finding out. We now ask for that header
  (`getcontentFeatures.dlna.org: 1`) and do not read the answer. Modelling those as distinct
  capabilities, with `sp-flag` making seek unrepresentable, is still the ground-rule-1
  shape when somebody wants it.
- **`res@bitrate` is specified in bytes/second** and almost universally emitted as
  bits/second. Be lenient if it is ever read.
- **A MIME-only `protocolInfo` entry (no `DLNA.ORG_PN`) is legal and testable** under WHCK
  PROT-05 — but it obliges the renderer to decode everything in the certification table for
  that MIME. So G80's enumeration should grow only as the decoder does. A `video/*` glob
  has *no defined obligation set*, which is why no certified device publishes one.

## Claims the review made and then withdrew

Kept because both readings sounded plausible, and only the primary source settled them.

- **"Wildcard `SinkProtocolInfo` is fine, gmrender does it."** Wrong. gmrender's format
  string is `http-get:*:%s:*` where `%s` is a **concrete MIME type** — the wildcards are in
  the other three fields, not the MIME field. Became G80.
- **"Rejecting a leading `+` in `res@duration` is lenient handling of non-conformant
  input."** Wrong, and the opposite of the truth: `av:duration.cds1` is
  `[-+]?[0-9]+(:[0-5][0-9]){2}…`, so the sign is normative and *we* were violating it.
  Became G81.

## What changed after this review

The defects it found are closed — G66 and G68–G82, less G77's HEAD probe and the compositor
half of G67, both recorded in GAPS with their reasons. Two things are worth carrying
forward from *how* they closed rather than from what they were.

**The obvious-looking fix was the wrong one a third time.** G68 said "implement GENA", and
the first correct step was the opposite: refuse. The table above is why. Getting to the real
implementation meant keeping the property that made refusing right — that a control point is
never told something we cannot back up.

**Closing them turned up a defect none of them named.** `MediaClock::set_paused` took a lock
and then called a function that takes the same lock, so the first pause of any session
deadlocked the thread that asked — a tokio worker on the box, and the whole test suite here.
It had been hiding as a slow `cargo test --workspace` through two verification gates. It is
GAPS G83, and its lesson is the one this file exists for: the failure mode was silence, and
silence is indistinguishable from working.

## What the citations are worth

Read directly, and quotable: UPnP AV Architecture, AVTransport:1/:2/:3, RenderingControl,
ConnectionManager:1, ContentDirectory:1/:4, UDA 1.0 and 1.1, the `av.xsd` /
`didl-lite*.xsd` / `upnp.xsd` schemas, MS-DLNHND, ETSI TS 102 905, DLNA Guidelines
**Part 5**, and the Rygel and gmrender-resurrect sources.

**Not obtained: DLNA Guidelines Part 1 (IEC 62481-1), which is genuinely paywalled.**
Every `7.4.x` / `7.5.4.3.2.x` section number in these docs is therefore a citation *made
by a reference implementation or by Microsoft*, not text anybody here has read. Treat them
as pointers, not as quotations.

Also: **two DLNA numbering schemes are in circulation** — the 2006 `7.3.x`/`7.4.x` and the
2011/2014 `7.5.4.3.2.x`. Do not mix them within a document.

## The archive

The primary sources are preserved at **`~/dlna-spec-archive/`** (111 MB): the four XSDs,
the specification PDFs and extracted text, and the Rygel and gmrender-resurrect trees.
They are outside the repository because most are third-party documents whose
redistribution terms nobody has checked.

**The XSDs are the exception worth checking in.** `av.xsd` is the normative grammar behind
`didl.rs::parse_duration` and settled G81 on its own; under ground rule 6 it belongs
in-tree as the basis for `res@duration` and DIDL golden tests, which is exactly the kind of
fixture ground rule 9 asks for. Not done yet.

Captured `CurrentURIMetaData` blobs from real control points belong in-tree for the same
reason, and do not exist yet — a real blob wrapped in CDATA is what would have caught G71
without a review.
