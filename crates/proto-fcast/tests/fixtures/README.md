# FCast wire fixtures

Byte transcripts of the **reference FCast sender** (the terminal sender from
`gitlab.futo.org/videostreaming/fcast`, built at the repository state of 2026-08-03,
linking `fcast-sender-sdk` 0.3.0 — the same SDK Grayjay embeds) driven against a
scripted receiver that answers exactly one thing: `Version {3}` on connect, plus
`Pong` for `Ping`. Captured 2026-08-09 per ground rule 9: the reference
implementation is the wire-behavior spec, and these are its recorded answers.

One JSONL file per sender invocation; rows are
`{"dir": "in"|"out", "t_ms": <ms since accept>, "hex": <whole frame>}` where `in` is
sender→receiver and `hex` includes the 4-byte little-endian size prefix.

Every transcript shows the SDK's connection preamble, none of which is optional and
only some of which the v3 spec mentions:

1. sends `Version {"version":4}` immediately (before reading ours),
2. reads our `Version {3}` and downgrades the session to v3,
3. sends `Initial {"appName":"FCast Sender SDK v0.3.0","appVersion":"0.3.0"}`
   (no `displayName`),
4. **auto-subscribes to `MediaItemEnd`** (`SubscribeEvent {"event":{"type":1}}`),
5. waits ~2 s (its "connected event deadline") before the actual command.

The `listen-events` transcript also records the scripted receiver's
`PlaybackUpdate`/`VolumeUpdate`/`PlaybackError` pushes (`out` rows) that the sender
accepted without complaint, and shows the sender sending **no** `Ping` of its own
over a 9 s idle window — the heartbeat is receiver-initiated in practice.

`client-2024-play.jsonl` is a different implementation entirely: nixpkgs'
`fcast-client` (0.1.0-unstable-2024-05-23, the pre-SDK terminal client). It sends
**no `Version` at all** — its first frame is the `Play` — and writes every optional
field explicitly (`"content":null`, `"time":0.0`, `"speed":1.0`), which is exactly
the implicit-v1 path and the null-tolerance the session claims to handle.

The `sdk-0.3.0-v4-*` transcripts are the same sender run through a **protocol v4**
session (#248): the scripted receiver advertised `v=4` and an `fp` TXT over mDNS
(the SDK refuses v4 without a fingerprint to pin — verified: by bare IP it closes
before the ClientHello), answered `Version {4}`, and terminated TLS 1.3 with the
pinned self-signed cert. Rows gain a `"plaintext"` field; TLS-phase frames are
recorded decrypted. Every one shows the v4 preamble — `SenderIntroduction` then an
automatic `CompanionHelloRequest`, both FlatBuffers under opcode 20 — and
`v4-set-playlist-item` catches the SDK sending the **raw v3 JSON opcode 16 inside a
v4 session**, which a receiver must answer with `Error{InvalidOpcode}` rather than
honour.

`tests/real_sender_transcripts.rs` replays every `in` frame through the pure
session + player and asserts what the session must have concluded. Recapture with a
newer sender by re-running the harness in the #241 work notes and adding files under
a new `sdk-<version>-` prefix — do not overwrite these.
