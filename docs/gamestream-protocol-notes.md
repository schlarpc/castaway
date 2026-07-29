# GameStream / Moonlight — protocol record

What `proto-gamestream` is built from. Read this before changing that crate.

This protocol is **inverted** relative to everything else in the project: castaway is the
*client* (the Moonlight role) and the peer — Sunshine, or a legacy GeForce Experience host
— is the server. Nothing arrives unbidden; the panel browses, dials, and asks.

It is also **split**, per DECISION-LOG D37:

| Half | Owner | Where |
|---|---|---|
| mDNS discovery, NVHTTP (`/serverinfo`, `/pair`, `/applist`, `/launch`, `/cancel`), the gen-7 pairing crypto, the client identity, the adapter | **ours** | `crates/proto-gamestream` |
| RTSP handshake, ENet control stream, FEC'd RTP video, encrypted Opus audio, input encoding | **linked** | moonlight-common-c, via `crates/moonlight-sys` |

So §1–§4 below are the specification our code is written against and must keep matching.
§5–§7 describe the linked half: not a build guide, but what you need to read a log or a
capture when a session misbehaves, and the boundary conditions our side has to satisfy for
that half to work at all.

**Sources.** Everything here was derived from source, at pinned revisions:
moonlight-common-c `e41355ea01670fd4c830b384009d31dd0339a705` (with the `cgutman/enet` fork
at `aca8784` and `nanors` at `b1e3c22`), Sunshine at `965c91a`, and moonlight-qt for the
client-side reference. Citations in the appendices are `path:line` into those trees — the
same revisions `flake.nix` pins, so they can be checked out and read. Nothing here is a
runtime dependency except moonlight-common-c itself, which D37 records as a deliberate
exception to ground rule 9.

---

## 1. Ports, and where they come from

Sunshine derives every port from one configurable base (default 47989):

| Port | Offset | What |
|---|---|---|
| 47989 | base+0 | NVHTTP, plaintext. `/serverinfo` and `/pair` only. |
| 47984 | base−5 | NVHTTP over TLS. Everything else. |
| 48010 | base+21 | RTSP. |
| 47998 | base+9 | Video, UDP. |
| 47999 | base+10 | Control (ENet), UDP. |
| 48000 | base+11 | Audio, UDP. |

Only the first is safe to assume. **The TLS port is read from `/serverinfo`'s `HttpsPort`**
and the RTSP endpoint from `/launch`'s `sessionUrl0`, because a host with a non-default base
port answers on none of the numbers above. `nvhttp::DEFAULT_HTTPS_PORT` exists as a fallback
for a host that omits the field, not as a default to reach for.

## 2. The client identity *is* the credential

There is no account and no password. Pairing teaches a host to trust one specific
certificate, and every later HTTPS request authenticates by presenting it. Lose the key and
you re-pair with every host.

We reproduce Moonlight's shape rather than exercising Sunshine's tolerance: RSA-2048,
self-signed, `CN=NVIDIA GameStream Client`, SHA-256. Sunshine accepts *expired* client
certificates deliberately (matching GFE), so validity is not load-bearing — but the
certificate bytes are, because pairing hashes them.

`uniqueid` is a separate thing: lowercase hex of a random u64, generated once and persisted,
sent on every request. Sunshine keys its in-flight pairing session on it, so all four pairing
calls must carry the same value. It is *not* a secret and *not* an authenticator — the TLS
certificate is.

TLS is **pinned, not validated**. The host certificate is self-signed and arrived over
plaintext HTTP during pairing, so the only check that means anything is byte equality against
that one certificate. A webpki-roots check would reject every host; skipping verification
would accept any. See `http.rs`.

## 3. Pairing (gen 7+, the Sunshine path)

Four plaintext round trips, then one over TLS. Full byte layouts in Appendix A §3; the
shape and the traps:

**Key derivation.** `aesKey = SHA-256(salt ‖ PIN-as-ASCII)[..16]`, used as **AES-128-ECB
with padding disabled**. The PIN is the ASCII text of the digits, so leading zeros matter.
Which hash is chosen depends on the host's `appversion` major (≥7 → SHA-256); Sunshine
advertises `7.1.431.-1` and is SHA-256-only, so a SHA-1 client can never pair with it.

**The "certificate signature".** Both sides hash the X.509 `signatureValue` BIT STRING
contents — the raw 256 signature bytes at the end of the DER — not a digest of the
certificate and not the TBS bytes. Getting this wrong produces a handshake that completes
every step and fails only at the end, with no indication of which field was wrong.

**The phases.**
1. `getservercert` — we send a salt and our certificate; the host returns its own.
2. `clientchallenge` — we send `ECB(random16)`; the host returns
   `ECB(SHA-256(ourChallenge ‖ itsCertSig ‖ itsSecret) ‖ itsChallenge)`.
3. `serverchallengeresp` — we send `ECB(SHA-256(itsChallenge ‖ ourCertSig ‖ ourSecret))`;
   the host returns `itsSecret ‖ RSA-sign(itsSecret)`.
4. `clientpairingsecret` — we send `ourSecret ‖ RSA-sign(ourSecret)`; the host says paired.
5. `phrase=pairchallenge`, **over TLS** — the first mutual-TLS request with the new
   certificate.

**Both trust checks happen at phase 3, deliberately.** That is the first moment we hold
enough to run either: the host's signature over its secret (a failure is a MITM or a host
that lost its key) and the hash it committed to in phase 2 (a mismatch is the PIN). Our code
keeps these apart as `Pairing` and `WrongPin` because the recoveries differ — retype the PIN,
versus stop trusting this host.

**Four ways this fails silently, all of which our code guards:**
- **Phase 1 must not have a timeout.** Sunshine holds the response open until someone types
  the PIN into its web UI. That is a wait to allow, not a timeout to tune.
- **Phase 4 reports refusal as `status_code=200` with `<paired>0</paired>`.** Reading the
  status alone calls a refusal success.
- **Phase 5 is the only proof that any of it worked.** The certificate does not enter the
  host's live TLS trust set until its next verify callback drains a queue, so a client that
  trusted phase 4 could go on to 401 on every request with nothing saying why. If phase 5
  fails for *any* reason, including transport, the pairing must be discarded — this was a
  real bug caught by the socket test.
- **An empty `plaincert` means another pairing is already in flight**, not a broken host.

**Hex conventions are asymmetric.** We emit lowercase, natural byte order; Sunshine emits
uppercase. Its parser is also lenient in a way worth not relying on: it accepts any ASCII
letter as a hex digit after `ch |= 0x20`, so non-hex letters are mis-decoded rather than
rejected. We decode strictly and emit strictly.

**There is no `/unpair` on Sunshine.** Moonlight calls it on every failure path; against
Sunshine that 404s harmlessly. Do not build recovery on it.

## 4. NVHTTP

Every response is XML whose `<root>` carries `status_code`. `200` is success; anything else
is an error *with a body*, and the body's `status_message` is the host's own explanation
("An app is already running on this host", "Is a display connected and turned on?"), which
is the text worth showing a person. `401` specifically means "not paired" — Sunshine writes
it without reading the request when client-certificate verification fails.

Fields we make decisions from:
- **`appversion`** — its major selects the pairing hash; a negative fourth component is
  Sunshine self-identifying.
- **`HttpsPort`** — see §1.
- **`PairStatus`** — only meaningful over TLS. Over HTTPS Sunshine sets it to 1 for the mere
  *presence* of a `uniqueid` parameter, so it is a hint, not a gate; the real gate is whether
  TLS succeeded at all.
- **`currentgame` + `state`** — decides `/launch` versus `/resume`. `currentgame` is only
  believed when `state` ends in `_SERVER_BUSY`, because GFE 2.8+ leaves it set to the last
  game played after it exits; believing it would send `/resume` to a host with nothing to
  resume.
- **`ServerCodecModeSupport`** — a bitmask; absent means H.264 only.

`/launch` parameters that matter (full list in Appendix A §4.3):
- **`rikey`/`rikeyid`** — the AES key and IV for input, control, and audio. `rikeyid` is the
  first four IV bytes read as a **signed** big-endian int32, so it is negative about half the
  time. Emitting it unsigned still parses on the host; it just builds a different IV, and the
  session then fails to decrypt with no error anywhere.
- **`corever=1`** — asks for the encrypted control/RTSP protocol. A host in mandatory-
  encryption mode returns 403 without it. It also changes `sessionUrl0`'s scheme to
  `rtspenc://`, which is how the streaming core learns RTSP is encrypted.
- **`surroundAudioInfo`** — `(channelMask << 16) | channelCount`; 196610 is stereo.
- **`mode=WxHxFPS`**, **`sops`**, **`localAudioPlayMode`**.

## 5. What the linked core does, and what we must hand it

`LiStartConnection` takes the `sessionUrl0` we got from `/launch`, the `appversion` and
`ServerCodecModeSupport` from `/serverinfo`, and the `rikey`/IV we generated, and runs the
rest: RTSP setup, the ENet control stream, both UDP media sockets, FEC, decryption, and
input. Appendices B–D are the record of what it does on the wire.

**The integration constraints that shape our code:**

- **It is a process singleton.** Not thread-safe, global state, and several callbacks
  (`DecoderRendererStart`, `DecoderRendererStop`) take no context pointer at all — there is
  nowhere to hang a per-session handle. One session per process, enforced in `stream.rs`.
- **`LiStartConnection` blocks** through the whole handshake, so it runs on `spawn_blocking`.
- **Callbacks arrive on the library's own threads**, which are not tokio threads. That makes
  a tokio channel send from them a *blocking* send — what we want for audio (back-pressure)
  and not for video, where a late frame is dropped instead (ground rule 4).
- **Video arrives as Annex-B already**, with SPS/PPS/VPS as the leading buffers of an IDR,
  which is exactly what the pipeline's decoder wants. Nothing is rewritten.
- **Audio arrives as raw Opus packets** at a fixed 5 ms cadence with no timestamps, so the
  presentation clock is counted on our side.
- **`logMessage` is left null.** It is a printf-style variadic, which Rust cannot define on
  stable; the library substitutes its own default and its messages duplicate the stage
  callbacks.

## 6. Where the remaining risk is

This is the section to read first when something is wrong.

**No session has ever run against a real Sunshine host.** The pairing half is proven against
Sunshine's own vectors and against a scripted host over real sockets; the streaming half is
proven to link and answer its own queries. Everything between "the host said 200 to /launch"
and "pixels" is unverified in this tree.

**There is no chooser.** Nothing here can yet put a list of hosts on the panel and take a
touch on it, so a session can only be started from config — which makes GameStream
operator-configured rather than walk-up, the opposite of this project's premise. D37 records
this as deferred, not designed.

**The GPL boundary.** moonlight-common-c is GPL-3.0 and this workspace is MIT. The `stream`
feature that links it is off by default at the cargo level and off in `castaway-portable`.
The Linux kiosk package turns it on, which makes that *artifact* GPL-3.0-bound — a deliberate
act with a licence consequence, taken for the panel build only; see D37.

**Things that will look like our bug and are not** (from the appendices, worth knowing):
- Sunshine's `not_found` handler writes its body twice, so a 404 body is unparseable. That is
  usually the real message "you asked an HTTPS-only endpoint over plaintext".
- SOF/EOF flags in the video packet header are per *FEC block*, not per frame.
- Audio FEC uses a parity matrix hardcoded on both sides that is **not** nanors' default —
  a from-scratch Reed-Solomon implementation would silently fail to recover.
- Sunshine reports `IsHdrSupported` per host, not per app, despite it living on the app
  element.

---

# Appendix A — pairing and NVHTTP (the half we implement)

Scope: everything a clean-room **client** (Moonlight role) must emit and verify to pair with and
drive a **Sunshine** host. Derived from source only.

Source roots (abbreviated in citations):
- `S/` = `Sunshine/` (server we must interoperate with)
- `M/` = `moonlight-qt/` (client reference)
- `LC/` = `moonlight-common-c` — **not present in this checkout** (`M/moonlight-common-c/moonlight-common-c`
  is an empty submodule dir; see `M/.gitmodules`). Facts attributed to `LC/` were read from
  upstream `master` on raw.githubusercontent.com and are marked as such.

---

### 1. Ports and transports

#### 1.1 Port arithmetic

Sunshine derives every port from one configured base port:

- Base port default `47989` — `S/src/config.cpp:876` (`47989,  // Base port number`).
- `net::map_port(offset)` returns `(uint16_t)(config::sunshine.port + offset)` — `S/src/network.cpp:224-233`.
- Offsets: `nvhttp::PORT_HTTP = 0` (`S/src/nvhttp.h:40`), `nvhttp::PORT_HTTPS = -5` (`S/src/nvhttp.h:45`),
  `rtsp_stream::RTSP_SETUP_PORT = 21` (`S/src/rtsp.h:15`), `stream::VIDEO_STREAM_PORT = 9`,
  `CONTROL_PORT = 10`, `AUDIO_STREAM_PORT = 11` (`S/src/stream.h:19-21`).

With the default base: **HTTP 47989, HTTPS 47984, RTSP 48010, video 47998, control 47999, audio 48000.**

Client-side defaults agree: `#define DEFAULT_HTTP_PORT 47989`, `#define DEFAULT_HTTPS_PORT 47984`
— `M/app/backend/nvaddress.h:5-6`.

The client must not hardcode HTTPS: it reads `HttpsPort` from the HTTP `/serverinfo` response and
falls back to 47984 if absent/zero (`M/app/backend/nvhttp.cpp:183-187`, `M/app/backend/nvcomputer.cpp:179-182`).

#### 1.2 Endpoint → transport map (Sunshine routing table, `S/src/nvhttp.cpp:1359-1382`)

| Path | HTTP 47989 | HTTPS 47984 |
|---|---|---|
| `/serverinfo` | yes (`:1379`) | yes (`:1360`) |
| `/pair` | yes (`:1380-1382`) | yes (`:1361-1363`) |
| `/applist` | no | yes (`:1364`) |
| `/appasset` | no | yes (`:1365`) |
| `/launch` | no | yes (`:1366-1368`) |
| `/resume` | no | yes (`:1369-1371`) |
| `/cancel` | no | yes (`:1372`) |
| anything else | `not_found` (`:1378`) | `not_found` (`:1359`) |

**There is no `/unpair` endpoint in Sunshine.** Moonlight calls `unpair` over HTTP on every pairing
failure path (`M/app/backend/nvpairingmanager.cpp:248,257,276,283,...`); against Sunshine this hits
`not_found` and returns a 404 body. Harmless, but a reimplementation should not depend on it.

All requests are `GET` with query-string arguments; there are no request bodies.

#### 1.3 TLS requirements

Server side (`S/src/nvhttp.cpp:53-136`, `1287-1357`):

- Context is `tls_server` with TLS 1.0 and 1.1 disabled (`:63-66`). Cert/key are
  `config::nvhttp.cert` / `config::nvhttp.pkey` (`:67-68`, `:1287`).
- `after_bind()` sets `verify_peer | verify_fail_if_no_peer_cert | verify_client_once`
  (`:80-88`). **A client certificate is mandatory on every HTTPS request**, including the
  first `/serverinfo` probe.
- The OpenSSL `verify_callback` unconditionally `return 1` (`:83-86`) — the comment says "To respond
  with an error message, a connection must be established". So the *handshake always succeeds*;
  authorization happens after.
- After a successful handshake (`:115-130`) Sunshine calls `https_server.verify(SSL*)`
  (`:1291-1342`):
  1. `SSL_get1_peer_certificate`; null → deny (`:1292-1302`).
  2. Drain the `add_cert` queue (certs newly accepted by pairing) into `cert_chain` (`:1314-1322`).
  3. `cert_chain.verify(x509)` — `S/src/crypto.cpp:64-93`. Each paired client cert is its own
     `X509_STORE` (one store per cert, to work around single-anchor verification —
     `S/src/crypto.cpp:54-63`); the presented cert is verified against each store in turn with
     `X509_V_FLAG_PARTIAL_CHAIN` (`S/src/crypto.cpp:77`, "We don't care to validate the entire
     chain for the purposes of client auth"). The verify callback whitelists
     `X509_V_ERR_CERT_NOT_YET_VALID` and `X509_V_ERR_CERT_HAS_EXPIRED`
     (`S/src/crypto.cpp:38-52`) — **expired client certs are accepted**, matching GFE.
  4. `is_client_enabled(pem)` where `pem = crypto::pem(x509)` (re-serialized by OpenSSL)
     (`S/src/nvhttp.cpp:1332-1336`). Lookup is **exact PEM string equality** against the stored
     `cert` field (`S/src/nvhttp.cpp:1468-1476`); if no entry matches it **returns `true`**
     (fail-open) — the real gate is step 3.
  5. On success stores `last_verified_client_cert = pem` in a global (`:1338`, declared `:178`)
     which `/launch` and `/resume` later copy into the launch session (`:407`).
- On verification failure, `on_verify_failed` (`:1344-1357`) writes, without reading the request:
  `root@status_code = 401`, `root@query = req->path`, `root@status_message = "The client is not
  authorized. Certificate verification failed."`.

Client side:

- Moonlight attaches its client cert + key to **every** request, HTTP and HTTPS alike
  (`M/app/backend/nvhttp.cpp:503` → `IdentityManager::getSslConfig()`,
  `M/app/backend/identitymanager.cpp:197-204`).
- The server cert is **pinned**, not CA-validated: `handleSslErrors` ignores every SSL error whose
  `error.certificate()` equals the pinned `m_ServerCert`, and refuses otherwise
  (`M/app/backend/nvhttp.cpp:435-455`). During pairing the pin is set from `plaincert` before any
  HTTPS request is made (`M/app/backend/nvpairingmanager.cpp:261-263`).
- A `SslHandshakeFailedError` is translated into a synthetic `GfeHttpResponseException(401,
  "Server certificate mismatch")` (`M/app/backend/nvhttp.cpp:554-560`), which drives the
  HTTPS→HTTP fallback for `/serverinfo` (`M/app/backend/nvhttp.cpp:153-170`).
- HTTP/2 is explicitly disabled and connection reuse is suppressed ("We must not keep persistent
  connections or GFE will puke") — `M/app/backend/nvhttp.cpp:505-515`.

---

### 2. Client identity

`M/app/backend/identitymanager.cpp:29-107` (`createCredentials`):

- Key: **RSA 2048**, `EVP_RSA_gen(2048)` on OpenSSL 3, else `EVP_PKEY_CTX_set_rsa_keygen_bits(ctx,
  2048)` (`:34-50`).
- Cert: `X509_set_version(cert, 2)` → **X.509 v3** (`:52`); serial number **0**
  (`ASN1_INTEGER_set(X509_get_serialNumber(cert), 0)`, `:53`).
- Validity: `notBefore = now`, `notAfter = now + 60*60*24*365*20` ("20 yrs", `:55-60`). Note this
  is computed as `int` seconds, so it is 20×365 days, not calendar years.
- Subject == Issuer == a single RDN: `CN=NVIDIA GameStream Client` (`:64-71`). No SANs, no
  extensions, no basicConstraints.
- Self-signed with `X509_sign(cert, pk, EVP_sha256())` (`:73`) → signature algorithm
  `sha256WithRSAEncryption`, signature value 256 bytes.
- Persisted as PEM: `PEM_write_bio_PrivateKey` (PKCS#8 `BEGIN PRIVATE KEY`, `:77`) and
  `PEM_write_bio_X509` (`:81`). On Darwin the key is re-emitted as PKCS#1 traditional for
  SecureTransport (`:181-185`) — not needed elsewhere.

**`uniqueid`** (`M/app/backend/identitymanager.cpp:137-151`):
- 8 random bytes read into a `uint64_t` via `RAND_bytes`, then `QString::number(uid, 16)` →
  **lowercase base-16, no zero padding, 1–16 chars**. Persisted under the settings key `"uniqueid"`.
- Sent as the **first query parameter of every request**, on every endpoint:
  `M/app/backend/nvhttp.cpp:496-498` builds
  `uniqueid=<id>&uuid=<32 hex>&<per-call args>`.
- Against GFE hosts Moonlight substitutes the literal **`0123456789ABCDEF`** ("Use a placeholder UID
  for GFE allow them to quit games for each other", `M/app/backend/nvhttp.cpp:496`). The real ID is
  used iff `!isNvidiaServerSoftware`, i.e. **for Sunshine the true ID is always sent**
  (`M/app/backend/nvhttp.cpp:43-46`, `m_UseTrueUid`).
- `uuid=` is a fresh random UUID per request: `QUuid::createUuid().toRfc4122().toHex()` → 32
  lowercase hex chars, no dashes (`M/app/backend/nvhttp.cpp:497`). Sunshine ignores it.

Sunshine's use of `uniqueid`:
- `/pair`: **mandatory**; it is the key of `map_id_sess` (`S/src/nvhttp.cpp:706-713,720,724,751-757`).
  All four HTTP pairing calls must carry the identical value.
- `/serverinfo` over HTTPS: mere *presence* of `uniqueid` sets `PairStatus=1`
  (`S/src/nvhttp.cpp:871-878`) — the value is never compared to anything.
- `/launch`,`/resume`: stored on the session, default `"unknown"` (`S/src/nvhttp.cpp:388`).

Sunshine's own identity: self-signed RSA-2048, `CN=Sunshine Gamestream Host`, 20-year validity,
random 159-bit serial, `X509_sign(..., EVP_sha256())` — `S/src/httpcommon.cpp:154-158` →
`crypto::gen_creds` `S/src/crypto.cpp:472-514`.

---

### 3. Pairing, step by step (gen 7+ / Sunshine path)

#### 3.0 Hash-algorithm selection rule

`M/app/backend/nvpairingmanager.cpp:209-225`:

```
serverMajorVersion = parseQuad(appVersion)[0]      // appVersion == /serverinfo <appversion>
if serverMajorVersion >= 7:  hashAlgo = SHA-256, hashLength = 32
else:                        hashAlgo = SHA-1,   hashLength = 20
```

`parseQuad` splits on `.` and `toInt()`s each component (`M/app/backend/nvhttp.cpp:95-114`).
Sunshine advertises `appversion = "7.1.431.-1"` (`S/src/nvhttp.h:30`), so **major = 7 → SHA-256
everywhere**. Sunshine itself is SHA-256-only: `crypto::hash()` is hardcoded to `EVP_sha256()`
(`S/src/crypto.cpp:350-354`) and phase 4 compares `hash.size() == clienthash.size()`
(`S/src/nvhttp.cpp:599`), so a SHA-1 client can never pair.

The `-1` in the 4th position is Sunshine's self-identification marker
(`S/src/nvhttp.h:26-30`, "The negative 4th number indicates to Moonlight that this is Sunshine").

#### 3.1 AES key derivation

Client (`M/app/backend/nvpairingmanager.cpp:227-231`):
```
salt      = 16 random bytes
saltedPin = salt || pin_as_utf8          // saltPin(), :200-204 — raw concat, no separator
aesKey    = SHA-256(saltedPin)[0..16]    // truncate(16)
```

Server (`S/src/crypto.cpp:334-348`, `gen_aes_key`): identical — `SHA256(salt_bytes || pin_ascii)`,
first 16 bytes. Called from `getservercert` (`S/src/nvhttp.cpp:463`).

PIN is the **ASCII text** of the digits, not a number. Sunshine's Web-UI entry point requires
exactly 4 characters, all `isdigit` (`S/src/nvhttp.cpp:780-797`); `/api/pin` additionally requires
`0 <= stoi(pin) <= 9999` (`S/src/confighttp.cpp:1372-1376`). Leading zeros are preserved because
the raw string is what gets hashed.

Sunshine takes the salt from the **first 32 characters** of the `salt` query value and requires
`salt.size() >= 32` (`S/src/nvhttp.cpp:454-461`); trailing junk is ignored.

#### 3.2 AES mode

**AES-128-ECB, padding disabled, on both sides.** No IV.

- Client: `EVP_aes_128_ecb()` + `EVP_CIPHER_CTX_set_padding(cipher, 0)`, single
  `EVP_EncryptUpdate`/`EVP_DecryptUpdate`, no `Final` call
  (`M/app/backend/nvpairingmanager.cpp:54-106`). Output length == input length; inputs are always
  multiples of 16.
- Server: `crypto::cipher::ecb_t cipher(*sess.cipher_key, false)` — the `false` is `padding`
  (`S/src/nvhttp.cpp:489`, `:541`; ctor `S/src/crypto.h:251`, impl `S/src/crypto.cpp:317-319`).
  `ecb_t::encrypt/decrypt` (`S/src/crypto.cpp:229-284`) size the buffer to
  `round_to_pkcs7_padded(n) = ((n+15)/16)*16` (`S/src/crypto.h:211-213`) then shrink to the real
  output length.

#### 3.3 Hex encoding conventions (critical)

| Direction | Encoder | Case | Byte order |
|---|---|---|---|
| client → server | `QByteArray::toHex()` (`M/app/backend/nvpairingmanager.cpp:236,270,301,348`) | **lowercase** | natural |
| server → client | `util::hex_vec(x, true)` (`S/src/nvhttp.cpp:467,517,552`) | **UPPERCASE** | natural |

- `util::hex_vec(begin, end, rev)` — `S/src/utility.h:565-602`. The digit table is `'0'..'9','A'..'F'`
  (uppercase). **The `rev` flag is inverted from what its name suggests**: `rev == true` walks
  `begin → end` (natural order); `rev == false` walks `end → begin` (reversed). Sunshine passes
  `true` for every pairing field, i.e. natural order.
- `util::from_hex_vec(hex, rev)` — `S/src/utility.h:688-737`. Decodes from the *end* of the string
  backwards into a forward buffer, then reverses iff `rev`; so again `rev == true` == natural order,
  which is what every pairing call site passes (`S/src/nvhttp.cpp:721,760,763,766`).
- `util::from_hex<T>(hex, rev)` — `S/src/utility.h:626-676`, same convention; used for `salt` into
  `std::array<uint8_t,16>` (`S/src/nvhttp.cpp:461`).
- **Sunshine's hex parser is lenient and buggy**: `is_convertable` returns true for *any* ASCII
  letter after `ch |= 0x20` (`'a' <= ch <= 'z'`), not just `a`–`f` (`S/src/utility.h:692-704`,
  `:626-640`). Non-hex letters are consumed and mis-decoded rather than rejected; other characters
  are skipped. Sunshine's own tests rely on this to embed `x` separators
  (`S/tests/unit/test_http_pairing.cpp:168,179`). A client should emit strictly `[0-9a-f]`.
- Moonlight decodes with `QByteArray::fromHex` (`M/app/backend/nvhttp.cpp:406-411`), which is
  case-insensitive.

Everything hex-encoded in this protocol is a **raw byte blob**; `plaincert`/`clientcert` are the
hex of the **PEM text** (including `-----BEGIN CERTIFICATE-----`, newlines, and trailing newline),
not of DER.

#### 3.4 Which signature bytes get hashed

Both sides hash the **X.509 `signatureValue` BIT STRING contents** — the raw signature at the end of
the DER certificate, *not* a hash of the cert, not the TBS bytes:

- Client: `X509_get0_signature(&asnSignature, NULL, cert)` then `ASN1_STRING_get0_data/length`
  (`M/app/backend/nvpairingmanager.cpp:108-129`; PEM variant `:131-148`).
- Server: `crypto::signature()` — same calls (`S/src/crypto.cpp:413-423`), used at
  `S/src/nvhttp.cpp:494-495` (server cert) and `S/src/nvhttp.cpp:587` (client cert).

For RSA-2048 + SHA-256 self-signed certs (both Moonlight's and Sunshine's) this is **256 bytes**.

#### 3.5 RSA signature scheme

`EVP_DigestSignInit(ctx, NULL, EVP_sha256(), NULL, key)` / `EVP_DigestVerifyInit(..., EVP_sha256(), ...)`
with no `EVP_PKEY_CTX` padding override → **RSASSA-PKCS1-v1_5 with SHA-256**, 256-byte signature.

- Client sign: `M/app/backend/nvpairingmanager.cpp:180-198`.
- Client verify: `M/app/backend/nvpairingmanager.cpp:150-178`.
- Server sign (`sign256`): `S/src/crypto.cpp:445-467`, `:519-521`.
- Server verify (`verify256`): `S/src/crypto.cpp:532-557`.

#### 3.6 The four HTTP `/pair` calls, plus the HTTPS confirmation

All five use `GET`. Every URL is prefixed by `uniqueid=<id>&uuid=<32 hex>&`
(`M/app/backend/nvhttp.cpp:496-498`). `devicename=roth` and `updateState=1` are sent on all five
(`M/app/backend/nvpairingmanager.cpp:235,269,300,347,360`) and are **ignored by Sunshine** —
Sunshine only ever reads `uniqueid`, `phrase`, `salt`, `clientcert`, `clientchallenge`,
`serverchallengeresp`, `clientpairingsecret` (`S/src/nvhttp.cpp:705-771`).

Sunshine enforces phase ordering via `pair_session_t::last_phase`
(`S/src/nvhttp.h:87-93,122`; checks at `S/src/nvhttp.cpp:448-452, 479-483, 529-533, 566-570`).
Any out-of-order call → `fail_pair` → `root@status_code=400`, `<paired>0</paired>`, and the session
is **deleted**, forcing a restart from phase 1 (`S/src/nvhttp.cpp:433-438`).

---

##### Phase 1 — `phrase=getservercert`

**Request** (HTTP 47989, `M/app/backend/nvpairingmanager.cpp:233-237`):

```
GET /pair?uniqueid=<id>&uuid=<32hex>
        &devicename=roth&updateState=1&phrase=getservercert
        &salt=<32 lowercase hex = 16 bytes>
        &clientcert=<hex of client PEM cert text, lowercase>
```

Timeout: **0 = infinite** (`:237`). This is required — Sunshine parks the response until a PIN is
entered.

**Server behaviour** (`S/src/nvhttp.cpp:716-743`): creates `pair_session_t{client.uniqueID,
client.cert = from_hex_vec(clientcert, true)}`, inserts into `map_id_sess`, stores the raw
`salt` string. Then either (a) `PIN_STDIN` flag set → prompts on stdin and answers immediately
(`:727-734`), or (b) **stashes the response object and returns without writing anything**
(`:736-742`, `fg.disable()`). The response is written later by `nvhttp::pin()` when the operator
POSTs `/api/pin` to the Web UI (`S/src/nvhttp.cpp:774-820`, `S/src/confighttp.cpp:1348-1382`,
route `^/api/pin$` at `S/src/confighttp.cpp:1811`).

`nvhttp::pin()` operates on `std::begin(map_id_sess)->second` — the **first** session in the map,
not a lookup by uniqueid (`S/src/nvhttp.cpp:799-801`) — and also sets `sess.client.name = name`,
which becomes the persisted device name.

Missing `clientcert` makes `get_arg` throw `std::out_of_range` (`S/src/nvhttp.cpp:217-227`); the
`fail_guard` still writes the (empty) tree, producing a body with no `<root>`. Moonlight reports
this as "Malformed XML (missing root element)" — Sunshine documents exactly this failure at
`S/src/nvhttp.cpp:43`.

**Response** (`getservercert`, `S/src/nvhttp.cpp:447-469`):

```xml
<root status_code="200"><paired>1</paired><plaincert>2D2D2D2D2D424547...</plaincert></root>
```

- `plaincert` = `util::hex_vec(servercert_PEM_text, true)` — UPPERCASE hex of the PEM **text**.
- Failure forms: `status_code=400`, `status_message="Out of order call to getservercert"` or
  `"Salt too short"`, `<paired>0</paired>`.

**Client checks** (`M/app/backend/nvpairingmanager.cpp:238-263`):
1. `verifyResponseStatus` → root `status_code` must be 200.
2. `<paired>` must equal `"1"`, else FAILED.
3. `plaincert` non-empty, else "Server likely already pairing" → `ALREADY_IN_PROGRESS`.
4. Parses as a certificate, else FAILED.
5. **Pins** the parsed cert for subsequent TLS (`m_Http.setServerCert(...)`, `:263`).

##### Phase 2 — `clientchallenge`

**Request** (HTTP 47989, timeout 5000 ms, `M/app/backend/nvpairingmanager.cpp:265-271`):

```
GET /pair?uniqueid=...&uuid=...&devicename=roth&updateState=1
        &clientchallenge=<32 lowercase hex = 16 bytes>
```

where `clientchallenge = AES-128-ECB-nopad(aesKey, randomChallenge[16])`.

**Server** (`clientchallenge`, `S/src/nvhttp.cpp:478-519`):

```
decrypted     = AES_dec(challenge)                      // 16 bytes
sign          = signature(server X509)                  // 256 bytes
serversecret  = rand(16)
hash          = SHA256( decrypted || sign || serversecret )     // 32 bytes   (:498-501)
serverchallenge = rand(16)
plaintext     = hash || serverchallenge                 // 48 bytes           (:504-508)
challengeresponse = AES_enc(plaintext)                  // 48 bytes           (:510-511)
```

`serversecret` and `serverchallenge` are retained on the session (`:513-514`).

**Response:**
```xml
<root status_code="200"><paired>1</paired><challengeresponse><96 UPPERCASE hex chars></challengeresponse></root>
```

**Client** (`M/app/backend/nvpairingmanager.cpp:272-296`):

```
challengeResponseData = AES_dec(aesKey, challengeresponse)      // 48 bytes
require len >= hashLength (32)                                   // :281-285
serverResponse  = challengeResponseData[0 .. 32]                 // the server's hash
serverChallenge = challengeResponseData[32 .. 48]                // 16 bytes
clientSecretData = rand(16)

challengeResponse = serverChallenge(16)
                 || signature(client X509)(256)
                 || clientSecretData(16)                          // 288 bytes  (:291-293)
paddedHash = SHA-256(challengeResponse)                           // 32 bytes
paddedHash.resize(32)                                             // no-op for SHA-256;
                                                                  // zero-pads 20→32 for SHA-1
serverchallengeresp = AES_enc(aesKey, paddedHash)                 // 32 bytes
```

Note `serverResponse` is *held* and only checked in phase 4 (after `pairingsecret` arrives).

##### Phase 3 — `serverchallengeresp`

**Request** (HTTP 47989, `M/app/backend/nvpairingmanager.cpp:298-302`):

```
GET /pair?uniqueid=...&uuid=...&devicename=roth&updateState=1
        &serverchallengeresp=<64 lowercase hex = 32 bytes>
```

**Server** (`S/src/nvhttp.cpp:528-555`):

```
sess.clienthash = AES_dec(serverchallengeresp)          // stored, NOT yet checked
sign            = sign256(server private key, serversecret)     // 256 bytes  (:548)
pairingsecret   = serversecret(16) || sign(256)                 // 272 bytes  (:550)
```

**Response:**
```xml
<root status_code="200"><pairingsecret><544 UPPERCASE hex chars></pairingsecret><paired>1</paired></root>
```

This phase **always** reports `paired=1`/`200` if ordering and state are valid — the client hash is
not validated here.

**Client checks** (`M/app/backend/nvpairingmanager.cpp:303-339`), in this exact order:

1. status 200 and `<paired> == "1"`, else stage-#3 FAILED.
2. `pairingsecret.size() > 16`, else "Invalid pairingsecret" → FAILED (`:311-316`).
3. `serverSecret = pairingsecret[0..16]`, `serverSignature = pairingsecret[16..]` (`:318-319`).
4. **MITM check**: `verifySignature(serverSecret, serverSignature, serverCertPem)` — RSA/SHA-256
   verify against the public key of the `plaincert` from phase 1. Failure → `"MITM detected"`,
   unpair, FAILED (`:321-328`).
5. **PIN check**: `SHA-256( randomChallenge(16) || signature(server X509 from plaincert)(256) ||
   serverSecret(16) ) == serverResponse` (the 32 bytes cached in phase 2). Failure → `PIN_WRONG`
   (`:330-339`).

Both failures issue the (nonexistent-on-Sunshine) `unpair` request.

##### Phase 4 — `clientpairingsecret`

**Request** (HTTP 47989, `M/app/backend/nvpairingmanager.cpp:341-349`):

```
GET /pair?uniqueid=...&uuid=...&devicename=roth&updateState=1
        &clientpairingsecret=<544 lowercase hex = 272 bytes>
```

where `clientpairingsecret = clientSecretData(16) || RSA-PKCS1v15-SHA256(client key, clientSecretData)(256)`.

**Server** (`S/src/nvhttp.cpp:565-613`):

```
require len > 16                                            (:574-577)
secret = cps[0..16] ; sign = cps[16..]                      (:579-580)
x509   = parse(client.cert) ; must parse                    (:582-586)
data   = serverchallenge(16) || signature(client X509)(256) || secret(16)   (:589-594)
same_hash = ( SHA256(data) == sess.clienthash )             (:596-599)
verify    = verify256(client X509, secret, sign)            (:600)
if same_hash && verify:
    <paired>1</paired>; add_cert->raise(x509); add_authorized_client(client.name, client.cert)
else:
    <paired>0</paired>
session removed either way; status_code = 200               (:601-612)
```

Note the failure case is **`status_code=200` with `<paired>0</paired>`** — the client must check
`<paired>`, not just the status code. Moonlight does (`M/app/backend/nvpairingmanager.cpp:351-356`).

`add_authorized_client` appends `{name, cert, uuid=random, enabled=true}` and calls `save_state()`
unless the `FRESH_STATE` flag is set (`S/src/nvhttp.cpp:343-354`).

The cert only enters the live TLS trust set on the **next** verify callback, which drains
`add_cert` (`S/src/nvhttp.cpp:1314-1322`) — this is why phase 5 exists and works.

##### Phase 5 — `phrase=pairchallenge` over **HTTPS**

**Request** (HTTPS 47984, `M/app/backend/nvpairingmanager.cpp:358-361`):

```
GET /pair?uniqueid=...&uuid=...&devicename=roth&updateState=1&phrase=pairchallenge
```

This is the first request that exercises mutual TLS with the freshly-paired client cert, against
the freshly-pinned server cert. Sunshine handles it *before* the session lookup and answers
unconditionally (`S/src/nvhttp.cpp:744-748`):

```xml
<root status_code="200"><paired>1</paired></root>
```

So this step tests exactly one thing: **does the TLS mutual auth now succeed**. If the cert was not
accepted the connection yields the 401 `on_verify_failed` body instead. On success, Moonlight
promotes the unverified cert to the persisted server cert (`:370`) and returns `PAIRED`.

#### 3.7 Byte-layout summary

| Field | Layout | Bytes | Hex chars |
|---|---|---|---|
| `salt` | random | 16 | 32 |
| `clientcert` | client PEM **text** | ~1200 | ~2400 |
| `plaincert` | server PEM **text** | ~1200 | ~2400 |
| `clientchallenge` | `ECB(clientChallenge[16])` | 16 | 32 |
| `challengeresponse` | `ECB( SHA256(clientChallenge‖serverCertSig‖serverSecret)[32] ‖ serverChallenge[16] )` | 48 | 96 |
| `serverchallengeresp` | `ECB( SHA256(serverChallenge[16]‖clientCertSig[256]‖clientSecret[16])[32] )` | 32 | 64 |
| `pairingsecret` | `serverSecret[16] ‖ RSA-SHA256(serverKey, serverSecret)[256]` | 272 | 544 |
| `clientpairingsecret` | `clientSecret[16] ‖ RSA-SHA256(clientKey, clientSecret)[256]` | 272 | 544 |

(256-byte signature sizes assume RSA-2048 on both ends, which is what both implementations
generate: `M/app/backend/identitymanager.cpp:35,42` and `S/src/httpcommon.cpp:158`.)

#### 3.8 Golden vectors (checked into Sunshine)

`S/tests/unit/test_http_pairing.cpp` is a deterministic, network-free exercise of the four server
phases and is directly reusable as a fixture source:

- Server key/cert PEMs: `:32-77` (a 2048-bit RSA key, `CN=localhost`, `O=GamesOnWhales`).
- `salt = "ff5dc6eda99339a8a0793e216c4257c4"`, `pin = "5338"` (`:144,148`).
- `clientchallenge` ciphertext `741CD3D6890C16DA39D53BCA0893AAF0` (`:150`, comment
  `AES("CLIENT CHALLENGE")`).
- `serverchallengeresp` ciphertext `920BABAE9F7599AA1CA8EC87FB3454C91872A7D8D5127DDC176C2FDAE635CF7A`,
  with the inline note that the plaintext SHA is
  `SHA256(server_challenge || public cert signature || "SECRET  ")` =
  `6493DAE49C913E1AEAF37C1072F71D664B72B2C4DA1FFB4720BECE0D929E008A` (`:151-153`).
- `override_server_challenge = 0xAAAAAAAAAAAAAAAA` (`:147`) — note this is only **8** bytes, so the
  hash-input layout is not length-prefixed and Sunshine will hash whatever length it holds.
- `clientpairingsecret` = 16-byte secret `000102030405060708090A0B0C0D0EFF` + a 256-byte signature
  (`:155-157`).
- Out-of-order enforcement is asserted at `:242-267`.

#### 3.9 Sunshine deviations from GFE visible in this source

- **SHA-256 only.** GFE gen<7 used SHA-1 (`M/app/backend/nvpairingmanager.cpp:220-225`); Sunshine's
  `crypto::hash` has no SHA-1 path (`S/src/crypto.cpp:350-354`).
- **No `/unpair`.** GFE has it (Moonlight calls it on failure); Sunshine returns 404.
- **PIN is entered on the host out-of-band** (Web UI `POST /api/pin` or stdin) and the phase-1
  response is held open indefinitely (`S/src/nvhttp.cpp:736-742`). GFE prompts on the host too, but
  a client must be prepared for an unbounded first request — Moonlight passes timeout `0`.
- **`PairStatus` is not really a pairing check**: any HTTPS `/serverinfo` bearing a `uniqueid`
  parameter reports `1` (`S/src/nvhttp.cpp:871-878`). The actual gate is TLS.
- **`ExternalPort` in `/serverinfo` is a Sunshine extension**, absent from GFE — stated verbatim at
  `M/app/backend/nvcomputer.cpp:184-189`.
- **`appversion` 4th component `-1`** is the Sunshine sentinel (`S/src/nvhttp.h:26-30`); Moonlight
  instead sniffs `state` for `MJOLNIR` to identify real NVIDIA software
  (`M/app/backend/nvcomputer.cpp:198-202`).
- Sunshine's `not_found` handler writes the XML body **twice** — once via `response->write(...)`
  and again after a manual `HTTP/1.1 404 NOT FOUND\r\n` status line
  (`S/src/nvhttp.cpp:672-681`). Treat 404 bodies as unparseable.

---

### 4. NVHTTP API surface

#### 4.0 Response envelope and error signalling

Every XML response is a boost `ptree` serialized with `pt::write_xml(data, tree)` using **default**
writer settings (e.g. `S/src/nvhttp.cpp:929-932`) — Sunshine never constructs an
`xml_writer_settings`. The exact prologue and whitespace are therefore boost library behaviour
(an XML declaration followed by the tree, indentation governed by the default `indent_count`);
this was **not verifiable from the sources in this checkout**. A client must not depend on
whitespace or the declaration — Moonlight feeds the whole body to a streaming XML reader
(`M/app/backend/nvhttp.cpp:413-433`) and matches on element names only.

Status convention: the single attribute **`status_code` on the `<root>` element**, plus an optional
`status_message` attribute. `200` = success. `S/src/nvhttp.cpp:884` etc.

Client-side rule (`M/app/backend/nvhttp.cpp:352-389`, `verifyResponseStatus`):
- Read `status_code` with an **unsigned** parse then cast to `int` — GFE 3.20.3 can emit
  `0xFFFFFFFF` (`:361-364`).
- `200` → OK; anything else throws with `status_message`.
- `401` is expected and non-noisy: it means "unpaired, fall back to HTTP".
- Special case: `status_code == -1 && status_message == "Invalid"` is remapped to 418 with a
  synthetic "missing audio capture device" message (`:377-382`).
- No `<root>` at all → `GfeHttpResponseException(-1, "Malformed XML (missing root element)")`.

Status codes Sunshine actually emits: `200`, `400` (missing/invalid params, out-of-order pairing,
app already running), `401` (TLS verify failed), `403` (mandatory encryption unmet), `404`
(unknown resource / invalid pairing request), `503` (encoder probe failed, nothing to resume), and
the raw `proc::proc.execute` error code for a failed app start (`S/src/nvhttp.cpp:1076`).

#### 4.1 `/serverinfo`

Handler `S/src/nvhttp.cpp:866-934`, templated over transport; both servers register it
(`:1360`, `:1379`).

Fields, in emission order:

| Element | Value | Cite |
|---|---|---|
| `@status_code` | `200` | `:884` |
| `hostname` | `config::nvhttp.sunshine_name` (default = host name) | `:885`, `S/src/config.cpp:828` |
| `appversion` | `"7.1.431.-1"` | `:887`, `S/src/nvhttp.h:30` |
| `GfeVersion` | `"3.23.0.74"` | `:888`, `S/src/nvhttp.h:35` |
| `uniqueid` | server UUID (`http::unique_id`), canonical lowercase 8-4-4-4-12 | `:889`, `S/src/uuid.h:59-86` |
| `HttpsPort` | `map_port(-5)` → 47984 | `:890` |
| `ExternalPort` | `map_port(0)` → 47989 | `:891` |
| `MaxLumaPixelsHEVC` | `"1869449984"` if `active_hevc_mode > 1` else `"0"` | `:892` |
| `mac` | real MAC over HTTPS; **`"00:00:00:00:00:00"` over HTTP** | `:896-900` |
| `LocalIP` | local endpoint address; forced to `127.0.0.1` for non-v4-mapped IPv6 | `:911-915` |
| `ServerCodecModeSupport` | decimal bitmask, see §5 | `:917-918` |
| `ExternalIP` | only present if `config::nvhttp.external_ip` non-empty | `:920-922` |
| `PairStatus` | `1` iff HTTPS **and** query has `uniqueid`; else `0` | `:870-878`, `:925` |
| `currentgame` | `proc::proc.running()` (app id, `0` when idle) | `:924,926` |
| `state` | `"SUNSHINE_SERVER_BUSY"` if `currentgame > 0` else `"SUNSHINE_SERVER_FREE"` | `:927` |

**Not emitted by Sunshine** (client must tolerate absence): `gputype`, `DisplayMode` list,
`ServerCodecModeSupport` sub-elements, `SupportedDisplayMode`.

Client fetch strategy (`M/app/backend/nvhttp.cpp:133-197`):
1. If a pinned server cert exists **and** `httpsPort() != 0` → try HTTPS first ("Always try HTTPS
   first, since it properly reports pairing status", `:143-149`). A `401` falls back to HTTP
   (`:153-164`); any other error is rethrown.
2. Otherwise HTTP, then read `HttpsPort` (default 47984 if 0/absent) and, if a cert is pinned,
   recurse into the HTTPS path (`:174-193`).

#### 4.2 `/applist` (HTTPS only)

Handler `S/src/nvhttp.cpp:956-982`. Response:

```xml
<root status_code="200">
  <App><IsHdrSupported>0|1</IsHdrSupported><AppTitle>…</AppTitle><ID>…</ID></App>
  …
</root>
```

- `IsHdrSupported` = `video::active_hevc_mode >= 3 ? 1 : 0` — **per-host, not per-app** (`:976`).
- Sunshine does **not** emit `IsAppCollectorGame`.
- An app with an empty name serializes as `<AppTitle/>`; Moonlight explicitly normalizes the
  resulting null string to `""` (`M/app/backend/nvhttp.cpp:324-335`).
- Client parses `App`, `AppTitle`, `ID`, `IsHdrSupported`, `IsAppCollectorGame`
  (`M/app/backend/nvhttp.cpp:300-350`) and throws "Invalid applist XML" if an `App` element closes
  without both a title and an ID (`:316-320`).

#### 4.3 `/launch` and `/resume` (HTTPS only)

**Full client-emitted query string** (`M/app/backend/nvhttp.cpp:200-246`, prefixed by
`uniqueid=&uuid=` from `:496-498`), in exact emission order:

```
uniqueid=<hex id>
&uuid=<32 hex>
&appid=<decimal>
&mode=<width>x<height>x<fps>
&additionalStates=1
&sops=<0|1>
&rikey=<32 lowercase hex = 16 bytes>
&rikeyid=<signed decimal int32>
[ &hdrMode=1&clientHdrCapVersion=0&clientHdrCapSupportedFlagsInUint32=0
  &clientHdrCapMetaDataId=NV_STATIC_METADATA_TYPE_1
  &clientHdrCapDisplayData=0x0x0x0x0x0x0x0x0x0x0        <- only when 10-bit is requested ]
&localAudioPlayMode=<0|1>
&surroundAudioInfo=<decimal>
&remoteControllersBitmap=<decimal>
&gcmap=<decimal>
&gcpersist=<0|1>
&corever=1
```

Details:

- Verb selection: `resume` if `currentGameId != 0`, else `launch`
  (`M/app/streaming/session.cpp:1614`).
- `mode`: `WxHxFPS`. Moonlight substitutes fps `0` when `fps > 60` **and the host is GFE**
  ("Using an FPS value over 60 causes SOPS to default to 720p60… We don't need this hack for
  Sunshine", `M/app/backend/nvhttp.cpp:221-225`). Against Sunshine the real fps is sent.
  Sunshine splits on `'x'` with `atoi`, default `"0x0x0"` (`S/src/nvhttp.cpp:372-387`).
- `rikey`: 16 raw AES key bytes, `QByteArray::toHex()` → **32 lowercase hex chars**
  (`M/app/backend/nvhttp.cpp:227`; the field is `char remoteInputAesKey[16]`, per `LC/Limelight.h`).
  Sunshine: `util::from_hex_vec(rikey, true)` → `launch_session->gcm_key`
  (`S/src/nvhttp.cpp:368-369`).
- `rikeyid`: the **first 4 bytes** of the 16-byte `remoteInputAesIv`, read as big-endian and
  emitted as a **signed decimal** (`memcpy` + `qFromBigEndian` + `QString::number`,
  `M/app/backend/nvhttp.cpp:210-213,228`) — so it can legitimately be negative. Sunshine parses it
  with `util::from_view` (`std::from_chars` into `int64_t`, `S/src/utility.h:815-817`), casts to
  `int`, byte-swaps to big-endian, and writes those 4 bytes into a 16-byte zero IV
  (`S/src/nvhttp.cpp:415-418`).
- `sops`: `1`/`0`. Against Sunshine Moonlight always forwards the user preference; against GFE it
  is suppressed unless the requested resolution appears in the host's `DisplayMode` list
  (`M/app/streaming/session.cpp:1585-1608`).
- `localAudioPlayMode`: `1` = play audio on the host too. Sunshine assigns it to a
  `host_audio` variable that is shared between `/launch` and `/resume`
  (`S/src/nvhttp.cpp:1037`, `:1285`).
- `surroundAudioInfo`: `SURROUNDAUDIOINFO_FROM_AUDIO_CONFIGURATION(x)` =
  `(channelMask << 16) | channelCount` (per `LC/Limelight.h`). Stereo → `(0x3<<16)|2` = **196610**,
  which is exactly Sunshine's default (`S/src/nvhttp.cpp:391`). 5.1 → `(0x3F<<16)|6` = 4128774;
  7.1 → `(0x63F<<16)|8` = 104529928.
- `remoteControllersBitmap` and `gcmap` carry the **same** attached-gamepad mask
  (`M/app/backend/nvhttp.cpp:234-235`). Sunshine reads only `gcmap` (`S/src/nvhttp.cpp:394`).
- `gcpersist` = `!multiController` (`M/app/streaming/session.cpp:1621`,
  `M/app/backend/nvhttp.cpp:236`). Sunshine ignores it.
- `corever=1` comes from `LiGetLaunchUrlQueryParameters()` (`M/app/backend/nvhttp.cpp:237`); per
  `LC/Connection.c` the function returns the literal `"&corever=1"`, with the comment
  `v0 = Video encryption and control stream encryption v2` / `v1 = RTSP encryption`.
  **Sunshine keys encrypted RTSP off this**: `corever >= 1` → `rtsp_cipher = AES-128-GCM(gcm_key)`,
  `rtsp_iv_counter = 0`, and the URL scheme becomes `rtspenc://` instead of `rtsp://`
  (`S/src/nvhttp.cpp:397-406`).
- `additionalStates`, `hdrMode`'s companions (`clientHdrCap*`) are ignored by Sunshine;
  `hdrMode` itself is read (`S/src/nvhttp.cpp:395`).
- Sunshine also honours `surroundParams` (string) and `continuousAudio` (`S/src/nvhttp.cpp:392-393`),
  which moonlight-qt does not send at this version.

**Required params.** `/launch` 400s unless `rikey`, `rikeyid`, `localAudioPlayMode`, **and** `appid`
are all present (`S/src/nvhttp.cpp:1013-1024`). `/resume` requires only `rikey` and `rikeyid`
(`S/src/nvhttp.cpp:1134-1144`); it reuses the stored `host_audio` unless there are no active
sessions and `localAudioPlayMode` was supplied (`:1146-1152`).

**`/launch` responses** (`S/src/nvhttp.cpp:991-1100`):

| Condition | Body |
|---|---|
| success | `<root status_code="200"><sessionUrl0>…</sessionUrl0><gamesession>1</gamesession></root>` (`:1084-1094`) |
| missing param | `status_code=400`, `status_message="Missing a required launch parameter"`, `<resume>0</resume>` (`:1019-1021`) |
| app already running | `status_code=400`, `"An app is already running on this host"`, `<resume>0</resume>` (`:1030-1032`) |
| encoder probe failed | `status_code=503`, `"Failed to initialize video capture/encoding. Is a display connected and turned on?"`, `<gamesession>0</gamesession>` (`:1054-1056`) |
| encryption mandatory, client lacks `corever` | `status_code=403`, `"Encryption is mandatory for this host but unsupported by the client"`, `<gamesession>0</gamesession>` (`:1062-1071`) |
| app start failed | `status_code=<proc error>`, `"Failed to start the specified application"`, `<gamesession>0</gamesession>` (`:1073-1082`) |

Note the odd `<resume>0</resume>` on two of the `/launch` error paths — that is Sunshine's element
name, not a typo here.

`appid == 0` is accepted and starts nothing (`if (appid > 0)`, `:1073`) but still returns success
and raises the session — this is the "desktop" case.

**`/resume` responses** (`S/src/nvhttp.cpp:1109-1198`): success is
`<root status_code="200"><sessionUrl0>…</sessionUrl0><resume>1</resume></root>` (`:1185-1195`);
`503 "No running app to resume"` when nothing is running (`:1125-1132`); `400 "Missing a required
resume parameter"`; `503` encoder probe; `403` mandatory encryption.

**`sessionUrl0` from Sunshine** (`S/src/nvhttp.cpp:1085-1093`, identical at `:1186-1194`):

```
std::format("{}{}:{}", rtsp_url_scheme, addr_to_url_escaped_string(local_endpoint.address()),
            map_port(RTSP_SETUP_PORT))
```

- scheme is `"rtsp://"` or `"rtspenc://"` (`S/src/nvhttp.cpp:406`);
- host is the **server's local endpoint address** for that connection, IPv6 wrapped in brackets
  (`S/src/network.cpp:167-176`);
- port is base+21 = **48010** by default.

Examples: `rtspenc://192.168.1.50:48010`, `rtsp://[fe80::1%25eth0]:48010`.

The client reads only `sessionUrl0` from the response (`M/app/backend/nvhttp.cpp:245`).

#### 4.4 `/cancel` (HTTPS only)

`S/src/nvhttp.cpp:1206-1229`. Takes no parameters, always succeeds:

```xml
<root status_code="200"><cancel>1</cancel></root>
```

Side effects: `rtsp_stream::terminate_sessions()`, `proc::proc.terminate()` if running, and
`display_device::revert_configuration()`.

Client (`M/app/backend/nvhttp.cpp:248-269`, 30 s timeout): after a 200 it **re-queries
`/serverinfo`** and, if `getCurrentGame() != 0`, throws a synthetic `GfeHttpResponseException(599,
"")` — because "Newer GFE versions will just return success even if quitting fails if we're not the
original requester." Against Sunshine `/cancel` genuinely terminates, but the client-side check is
cheap and should be kept.

#### 4.5 `/appasset` (HTTPS only)

`GET /appasset?uniqueid=…&uuid=…&appid=<id>&AssetType=2&AssetIdx=0`
(`M/app/backend/nvhttp.cpp:391-404`). Sunshine reads only `appid` and streams the PNG with
`Content-Type: image/png` (`S/src/nvhttp.cpp:1237-1248`). Not XML.

---

### 5. `/serverinfo` fields the client makes decisions from

**`state`** — two independent uses:
- `state.endsWith("_SERVER_BUSY")` gates whether `currentgame` is believed; otherwise the client
  forces current game to 0 (GFE 2.8+ leaves `currentgame` set to the last game played)
  — `M/app/backend/nvhttp.cpp:116-131`.
- `state.contains("MJOLNIR")` → `isNvidiaServerSoftware` (`M/app/backend/nvcomputer.cpp:196-202`).
  Sunshine's `SUNSHINE_SERVER_FREE`/`BUSY` never matches, so Sunshine is always treated as
  non-NVIDIA, which in turn: enables the true `uniqueid` (§2), disables the >60fps `mode` hack
  (§4.3), always forwards `sops`, and skips the >4K NVENC capability guard
  (`M/app/streaming/session.cpp:1218-1230`).

**`currentgame`** → `launch` vs `resume` verb (`M/app/streaming/session.cpp:1614`).

**`PairStatus`** → `PS_PAIRED` / `PS_NOT_PAIRED` (`M/app/backend/nvcomputer.cpp:204-205`).

**`appversion`** → pairing hash selection (§3.0) and is passed verbatim to the streaming core as
`SERVER_INFORMATION.serverInfoAppVersion` (`M/app/streaming/session.cpp:1631-1635`).

**`GfeVersion`** → passed to the core as `serverInfoGfeVersion`; also gates a macOS-only HEVC
workaround for GFE < 3.11 (`M/app/streaming/session.cpp:842-850`).

**`HttpsPort`** → the HTTPS port for all subsequent requests, default 47984 if absent/0
(`M/app/backend/nvcomputer.cpp:179-182`).

**`ExternalPort`** → WAN HTTP port; falls back to the current HTTP port. Explicitly documented as a
Sunshine-only extension (`M/app/backend/nvcomputer.cpp:184-189`).

**`ExternalIP`**, **`LocalIP`** (`127.*` is discarded), **`mac`** (`00:00:00:00:00:00` is discarded)
— `M/app/backend/nvcomputer.cpp:141-147, 171-177, 191-195`.

**`ServerCodecModeSupport`** — decimal bitmask; absent ⇒ assume `SCM_H264`
(`M/app/backend/nvcomputer.cpp:149-156`). Bit values (from `LC/Limelight.h`, upstream master;
**not present in this checkout**):

```
SCM_H264            0x00000001
SCM_HEVC            0x00000100
SCM_HEVC_MAIN10     0x00000200
SCM_AV1_MAIN8       0x00010000
SCM_AV1_MAIN10      0x00020000
SCM_H264_HIGH8_444  0x00040000
SCM_HEVC_REXT8_444  0x00080000
SCM_HEVC_REXT10_444 0x00100000
SCM_AV1_HIGH8_444   0x00200000
SCM_AV1_HIGH10_444  0x00400000
SCM_MASK_10BIT  = HEVC_MAIN10|HEVC_REXT10_444|AV1_MAIN10|AV1_HIGH10_444
SCM_MASK_YUV444 = H264_HIGH8_444|HEVC_REXT8_444|HEVC_REXT10_444|AV1_HIGH8_444|AV1_HIGH10_444
SCM_MASK_AV1    = AV1_MAIN8|AV1_MAIN10|AV1_HIGH8_444|AV1_HIGH10_444
```

Sunshine composes the value in `get_codec_mode_flags()` (`S/src/nvhttp.cpp:827-858`):
`SCM_H264` always; `+H264_HIGH8_444` if the probe found YUV444 for codec 0; `+HEVC` if
`active_hevc_mode >= 2` (`+HEVC_REXT8_444` with 444 support); `+HEVC_MAIN10` if
`active_hevc_mode` is 3 or 5; `+HEVC_REXT10_444` if mode 4 or 5 **and** 444; and the mirrored AV1
rules off `active_av1_mode`.

Client consumption (`M/app/streaming/session.h:60-88`, `maskByServerCodecModes`) maps SCM bits 1:1
onto `VIDEO_FORMAT_*` and intersects with the locally supported set; the mapping is asserted
exhaustive (`SDL_assert(serverCodecModes == 0)` at `session.h:84`). Decision points:
`SCM_MASK_AV1` (`session.cpp:988`), `SCM_MASK_10BIT` / HDR (`:1084`), `SCM_AV1_MAIN10`
(`:1092`), `SCM_HEVC_MAIN10` (`:1110`, and the >4K guard at `:1221`), `SCM_MASK_YUV444` (`:1139`).

**`MaxLumaPixelsHEVC`** — only consulted together with `isNvidiaServerSoftware` for the >4K guard
(`M/app/streaming/session.cpp:1218-1230`); irrelevant for Sunshine.

**Encryption flags are *not* surfaced in `/serverinfo`.** Encryption is negotiated by the client
asserting `corever=1` on `/launch`; the host may reject with `403` if
`config::ENCRYPTION_MODE_MANDATORY` applies to the peer's network class
(`S/src/nvhttp.cpp:1062-1071`, `:1174-1183`, `net::encryption_mode_for_address`).

---

### 6. Sunshine's paired-client persistence (for pre-seeding an integration test)

#### 6.1 Where

- Path: `config::nvhttp.file_state`, **default `sunshine_state.json`** relative to the working
  directory (`S/src/config.cpp:829`, override key `file_state` at `S/src/config.cpp:1694`; documented
  at `S/docs/configuration.md:1940-1961`).
- **The same file is also the Web-UI credentials file**: `config::sunshine.credentials_file =
  config::nvhttp.file_state` (`S/src/config.cpp:1697`, "Must be run after \"file_state\"").
  `username`/`salt`/`password` live at the JSON top level (`S/src/httpcommon.cpp:86-107`),
  alongside `root`.
- Server TLS credentials are separate files: `config::nvhttp.pkey` / `config::nvhttp.cert`, default
  `<CA_DIR>/cakey.pem` and `<CA_DIR>/cacert.pem` (`S/src/config.cpp:50-51,825-826`). They are
  auto-generated on first run if either is missing (`S/src/httpcommon.cpp:57-74,154-158`).

#### 6.2 When it is read

`load_state()` runs at `nvhttp::start()` **only if the `FRESH_STATE` flag is unset**
(`S/src/nvhttp.cpp:1271-1275`). With `FRESH_STATE`, `http::init()` also relocates cert/pkey into
`$TMP/Sunshine/{cert,pkey}-<uuid>` (`S/src/httpcommon.cpp:59-64`). **So the integration test must
not pass the fresh-state flag.**

#### 6.3 Shape

Writer (`save_state`, `S/src/nvhttp.cpp:232-267`):
1. Read the existing file if present (preserving any non-`root` keys).
2. `root.erase("root")` — the whole `root` subtree is replaced.
3. `root.put("root.uniqueid", http::unique_id)`.
4. For each paired device, a node with `name`, `cert`, `uuid`, `enabled`; all nodes are pushed with
   an **empty key** (`:257`), which boost serializes as a JSON **array** under
   `root.named_devices`.
5. `pt::write_json(file_state, root)` — boost's JSON writer emits **every scalar as a quoted
   string**, including booleans.

Reader (`load_state`, `S/src/nvhttp.cpp:272-335`):
- `root.uniqueid` must exist, else the file is treated as "not moonlight credentials" and a fresh
  server UUID is generated (`:288-294`).
- Legacy import path: `root.devices[].uniqueid` + `root.devices[].certs[]` (bare PEM strings,
  synthesizing a name of `""` and a random uuid) — `:299-315`. Still supported.
- Current path: `root.named_devices[]` with `name`, `cert`, `uuid` required and
  `enabled` optional defaulting to `true` (`:317-326`). `get<bool>` accepts both `"true"/"false"`
  and `"1"/"0"`.
- `cert_chain` is cleared and rebuilt from every `named_devices[].cert` (`:328-332`) — this is the
  TLS trust set.

**Minimal pre-seeded file:**

```json
{
    "root": {
        "uniqueid": "8b1f0e6c-3f2a-4c7d-9a11-2b6d5e4f7a90",
        "named_devices": [
            {
                "name": "castaway-test",
                "cert": "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----\n",
                "uuid": "4D7BB2DD-5704-A405-B41C-891A022932E1",
                "enabled": "true"
            }
        ]
    }
}
```

Constraints derived from the code:

- `cert` is the **PEM text with real newlines**, JSON-escaped as `\n`. It is parsed with
  `crypto::x509()` (`S/src/nvhttp.cpp:331`, `S/src/crypto.cpp:359-368`), so ordinary
  `PEM_write_bio_X509` output (64-char base64 lines, trailing newline) works.
- Store the **OpenSSL-canonical** PEM. `is_client_enabled` compares the stored string byte-for-byte
  against `crypto::pem(presented_x509)` (`S/src/nvhttp.cpp:1332-1336, 1468-1476`,
  `S/src/crypto.cpp:387-395`). A mismatch is fail-open (returns `true`), so it will not break the
  test — but only an exact match lets `enabled: false` ever work.
- `uniqueid` here is the **server's** UUID advertised in `/serverinfo`, unrelated to the client's
  `uniqueid` query param. Canonical lowercase 8-4-4-4-12 (`S/src/uuid.h:59-86`); the example above
  uses the uppercase form Sunshine's own docs show (`S/src/nvhttp.h:210`) — both parse.
- `uuid` on a device entry is Sunshine-internal (used by `/api/clients/unpair` and
  `/api/clients/update`, `S/src/nvhttp.cpp:1424-1451`); any string works.
- The cert must be **self-signed and verifiable as its own anchor** (`X509_verify_cert` with
  `X509_V_FLAG_PARTIAL_CHAIN`); expiry does not matter (`S/src/crypto.cpp:38-52,64-93`).
- Because `save_state()` rewrites the whole `root` subtree on the next successful pairing, a test
  that pre-seeds and then pairs again will lose nothing but should expect the file to be rewritten.

#### 6.4 Runtime alternatives to file seeding

- `POST /api/pin` with `{"pin":"1234","name":"My PC"}` completes phase 1 programmatically
  (`S/src/confighttp.cpp:1348-1382`, route `:1811`) — requires Web-UI auth + CSRF token
  (`:1354-1360`).
- `GET /api/clients/list`, `POST /api/clients/unpair`, `/api/clients/unpair-all`,
  `/api/clients/update` (`S/src/confighttp.cpp:1800-1803`; JSON shape
  `{name, uuid, enabled}` from `get_all_clients`, `S/src/nvhttp.cpp:936-948`).
- Starting Sunshine with the PIN-on-stdin flag makes phase 1 answer synchronously from a stdin read
  (`S/src/nvhttp.cpp:727-734`).

---

### 7. Implementation checklist for the Rust client

1. Generate RSA-2048 self-signed cert, `CN=NVIDIA GameStream Client`, v3, serial 0, 20-year
   validity, `sha256WithRSAEncryption`; keep the PEM text (it is hashed and hex-shipped verbatim).
2. Random `uniqueid` = lowercase hex of a `u64` (no padding); fresh random 32-hex `uuid` per request.
3. `GET http://host:47989/serverinfo` → read `HttpsPort`, `appversion`, `state`.
4. Pair over HTTP with the five steps in §3.6; **first request must have no timeout**.
5. Pin `plaincert`; use it as the sole trust anchor for HTTPS; present the client cert on every
   request including the pre-pairing `/serverinfo`.
6. Treat `status_code=401` as "not paired" and fall back to HTTP `/serverinfo`.
7. Emit all hex lowercase, natural byte order; parse incoming hex case-insensitively.
8. On `/launch`, always send `corever=1` unless deliberately opting out of RTSP encryption; parse
   `sessionUrl0` for the scheme (`rtsp://` vs `rtspenc://`) as well as the address.
9. Check `<paired>` text, not just `status_code`, on every pairing response — phase 4 signals
   failure with `status_code=200`.

# Appendix B — RTSP and the ANNOUNCE SDP (linked)

Sources (citations are `file:line`):
- `mlc` = moonlight-common-c @ e41355e — `src/RtspConnection.c`, `src/RtspParser.c`, `src/Rtsp.h`, `src/SdpGenerator.c`, `src/Connection.c`, `src/Limelight.h`, `src/Limelight-internal.h`, `src/Video.h`, `src/Misc.c`
- `sun` = Sunshine @ 965c91a — `src/rtsp.cpp`, `src/rtsp.h`, `src/stream.h`, `src/nvhttp.cpp`, `src/nvhttp.h`, `src/network.cpp`, `src/config.cpp`, `src/platform/common.h`

Version context: Sunshine reports `appversion = "7.1.431.-1"` (sun nvhttp.h:30, sent at nvhttp.cpp:887). Client parses it into `AppVersionQuad = {7,1,431,-1}` (mlc Misc.c:87-104). Therefore for Sunshine: `IS_SUNSHINE()` ⇔ `AppVersionQuad[3] < 0` is **true** (mlc Limelight-internal.h:85); `APP_VERSION_AT_LEAST(7,1,431)` is **true** (Limelight-internal.h:80-83). Everything below states the Sunshine-effective path first; GFE-legacy branches are flagged.

---

### 1. Transport

#### Port
- Client parses the RTSP port from the **last `:`** of `serverInfo->rtspSessionUrl` (`sessionUrl0` from the HTTP launch/resume response); fallback **48010** if absent/unparseable (mlc Connection.c:182-206, 268-280).
- Sunshine emits `sessionUrl0 = "{scheme}{addr}:{port}"` where scheme is `rtsp://` or `rtspenc://` (sun nvhttp.cpp:1084-1094, scheme chosen at nvhttp.cpp:406) and port = configured base port + 21 (sun rtsp.h:15 `RTSP_SETUP_PORT = 21`; network.cpp:223-233 `map_port` = base+offset). Default base is 47989 (sun config.cpp:876), so default RTSP port = **48010**.

#### TCP vs ENet
- `useEnet = (AppVersionQuad[0] >= 5) && (AppVersionQuad[0] <= 7) && (AppVersionQuad[2] < 404)` (mlc RtspConnection.c:950). Sunshine's quad is {7,1,431,-1} → 431 ≥ 404 → **useEnet = false; Moonlight always uses plain TCP RTSP with Sunshine**. The ENet path (`rtspru://` scheme, RtspConnection.c:982/987; connect at 1011-1045; one reliable packet for headers + a second packet for payload, and response likewise split, RtspConnection.c:247-384) applies only to GFE gen 5–7 builds < x.x.404. Sunshine's RTSP server is a boost::asio **TCP** acceptor only (sun rtsp.cpp:470-518, 733).
- Encrypted RTSP is explicitly unsupported over ENet (mlc RtspConnection.c:258-260).

#### Connection model / framing over TCP
- **One TCP connection per request.** `transactRtspMessageTcp` connects (10 s timeout, retrying every 500 ms on ECONNREFUSED for up to 10 s, mlc RtspConnection.c:4-6, 402-423), sends the serialized message, then reads until the **server closes the connection** (recv()==0) with a 15 s poll timeout, then closes (RtspConnection.c:445-517). Sunshine shuts down the socket after answering each request (sun rtsp.cpp:535-537). There is no pipelining and no interleaved `$` framing.
- Client caps responses at 1 MiB (mlc RtspConnection.c:7,452). Sunshine caps each **request** (header+payload, or encrypted header+payload) at its 2048-byte buffer (sun rtsp.cpp:459, 161-170, 224-229) — the ANNOUNCE must fit in 2048 bytes.
- Sunshine accepts a connection only while a launch session raised by the HTTP `launch`/`resume` handler is pending; otherwise the socket is closed immediately (sun rtsp.cpp:553-567). The pending session expires after `ping_timeout` (rtsp.cpp:603-612), so the handshake must begin promptly after launch.

#### Encrypted RTSP (`rtspenc://`)
- Client enables it iff `rtspSessionUrl` contains `"rtspenc://"` (mlc RtspConnection.c:955). Sunshine offers it iff the client's HTTP launch had `corever >= 1` (sun nvhttp.cpp:397-406). If Sunshine's config demands mandatory encryption and the client didn't send `corever>=1`, launch itself is rejected 403 (nvhttp.cpp:1063-1070).
- Framing: every message (request and response) becomes
  `struct ENC_RTSP_HEADER { uint32 typeAndLength; uint32 sequenceNumber; uint8 tag[16]; }` + ciphertext, where `typeAndLength = BE32(0x80000000 | plaintextLen)` and `sequenceNumber = BE32(seq)` (mlc RtspConnection.c:93-99,134-135; sun rtsp.cpp:59-99). The MSB distinguishes encrypted from plaintext (`ENCRYPTED_RTSP_BIT` 0x80000000, RtspConnection.c:93).
- Cipher: **AES-128-GCM**. Key = `StreamConfig.remoteInputAesKey` (16 bytes, mlc Limelight.h:101; RtspConnection.c:137-142) — this is the `rikey` the client generated and sent hex-encoded in the HTTP launch request; Sunshine decodes it into `gcm_key` (sun nvhttp.cpp:368-369) and builds the RTSP cipher from it (nvhttp.cpp:400-404).
- IV = 12 bytes, all zero except: bytes 0–3 = the 32-bit sequence number in **little-endian** (`iv[0]`=LSB), `iv[10]`/`iv[11]` = fixed field: `'C','R'` for client→host, `'H','R'` for host→client (mlc RtspConnection.c:123-132 encrypt, 197-206 decrypt; sun rtsp.cpp:266-277, 822-834). Client counter starts at 0 and is pre-incremented, so the first request has seq = 1 (RtspConnection.c:21,124); Sunshine's `rtsp_iv_counter` starts 0 and is pre-incremented likewise (nvhttp.cpp:404, rtsp.cpp:831). Directions keep independent counters.
- The GCM tag lives in the header (`tag[16]`); ciphertext follows the header. Client rejects responses without the MSB set, with truncated or excess data (RtspConnection.c:182-195).

#### Message grammar (plaintext layer, both directions)
Serializer (mlc RtspParser.c:336-407):
```
<COMMAND> <target> RTSP/1.0\r\n            (requests)
RTSP/1.0 <code> <status>\r\n               (responses)
<Option>: <content>\r\n                    (0..n)
\r\n
<payload bytes>                             (no trailing terminator)
```
CSeq is carried as an ordinary option, not via the parser's sequenceNumber field ("FIXME: Hacked CSeq attribute due to RTSP parser bug", mlc RtspConnection.c:77-79). The parser both sides share tolerates a missing final CRLF or two (RtspParser.c:172-198) and treats everything after `\r\n\r\n` as payload; Sunshine additionally reads `Content-length` to wait for the full payload (sun rtsp.cpp:367-393, digit-scan parse at 374-379).

---

### 2. Request sequence

State initialized per handshake: `currentSeqNumber = 1`, `controlStreamId = "streamid=control/13/0"` for ≥7.1.431 else `"streamid=control/1/0"` (mlc RtspConnection.c:951-953).

**Headers on every request** (`initializeRtspRequest`, mlc RtspConnection.c:72-91):
- `CSeq: <n>` — starts at 1, +1 per request, in request order.
- `X-GS-ClientVersion: <rtspClientVersion>` — 10/11/12/13/14 for AppVersionQuad[0] = 3/4/5/6/7+ (RtspConnection.c:990-1008); **14 for Sunshine**. Sunshine never reads it.
- `Host: <urlAddr>` (TCP only, RtspConnection.c:85). `urlAddr` = host part of `rtspSessionUrl` **when the client wants high-quality audio** (bitrate ≥ 15000 Kbps, not slow decoder, local-or-stereo — RtspConnection.c:959-984, threshold Limelight-internal.h:95); otherwise `urlAddr = "0.0.0.0"` (RtspConnection.c:985-988). Sunshine reads Host only in ANNOUNCE and only for 2-channel audio: stereo high-quality ⇔ Host does **not** contain `"0.0.0.0"` (sun rtsp.cpp:1211-1221).

`rtspTargetUrl` = the verbatim `rtspSessionUrl` string when high-quality audio is desired and parsing succeeded, else `rtsp://0.0.0.0:<port>` (constructed) (RtspConnection.c:969-988).

Exact order (mlc RtspConnection.c:1047-1413), each on its own TCP connection:

| # | Request | Target | Extra headers | Client parses from response |
|---|---------|--------|---------------|------------------------------|
| 1 | `OPTIONS` | `rtspTargetUrl` | — | status == 200 only (1051-1062) |
| 2 | `DESCRIBE` | `rtspTargetUrl` | `Accept: application/sdp`, `If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT` (559-562) | payload — see §3 |
| 3 | `SETUP` | `streamid=audio/0/0` (gen≥5; else `streamid=audio`, 1175-1176) | `Transport: unicast;X-GS-ClientPort=50000-50001` (gen≥6; literal `" "` for gen<6, 591-599), `If-Modified-Since: ...` (602) | `Session` (mandatory, 1214-1242), `Transport` `server_port=`, `X-SS-Ping-Payload` |
| 4 | `SETUP` | `streamid=video/0/0` (1252-1253) | `Session: <id>`, `Transport`, `If-Modified-Since` | `Transport` `server_port=`, `X-SS-Ping-Payload` |
| 5 | `SETUP` | `streamid=control/13/0` (gen≥5 only, 1289-1296) | same | `Transport` `server_port=`, `X-SS-Connect-Data` |
| 6 | `ANNOUNCE` | `streamid=control/13/0` (≥7.1.431; else `streamid=video`, 647-648) | `Session: <id>`, `Content-type: application/sdp`, `Content-length: <n>` (652-665) | status == 200 |
| 7 | `PLAY` | `/` (≥7.1.431, 1354-1358; legacy GFE: two PLAYs, `streamid=video` then `streamid=audio`, 1373-1412) | `Session: <id>` (626) | status == 200 |

`SETUP` targets are only sent with `Session` after one has been obtained (RtspConnection.c:584-589) — i.e. the audio SETUP has no Session header; the rest do.

**Session id**: Sunshine always answers SETUP with `Session: DEADBEEFCAFE;timeout = 90` (sun rtsp.cpp:1037-1038). The client takes the token before the first `;` — so the id used afterwards is literally `DEADBEEFCAFE` (mlc RtspConnection.c:1222-1234, comment cites exactly this Sunshine format).

**Ports from Transport**: client scans the `Transport` response header for `server_port=` and parses a decimal port (mlc RtspConnection.c:717-745). Sunshine sends only `Transport: server_port=<port>` (sun rtsp.cpp:1042-1046) with port = base + 11 / 9 / 10 for audio / video / control (sun stream.h:19-21 → defaults 48000 / 47998 / 47999). Client fallbacks if parsing fails: audio 48000, video 47998, control 47999 (mlc RtspConnection.c:1192-1200, 1276-1284, 1320-1328). Sunshine derives the stream type from the SETUP target by taking the text between the first `=` and the following `/` (sun rtsp.cpp:1017-1033) — unknown types get 404.

**Sunshine session-ID extensions** (only meaningful when client advertises `ML_FF_SESSION_ID_V1` in the later ANNOUNCE):
- `X-SS-Ping-Payload` on audio and video SETUP responses: 16 ASCII hex chars (hex of 8 random bytes, sun nvhttp.cpp:410-412, sent at rtsp.cpp:1054-1055). Client copies it only if `strlen == 16` into the 16-byte `SS_PING.payload` (mlc RtspConnection.c:1202-1207, 1267-1272; SS_PING struct Video.h:51-54) — used later as the UDP ping payload on the A/V ports.
- `X-SS-Connect-Data` on the control SETUP response: decimal `uint32` (random, sun nvhttp.cpp:413, rtsp.cpp:1049-1052); client `strtoul`s it (mlc RtspConnection.c:1310-1316) and echoes it in the ENet control connect.

Sunshine echoes `CSeq: <n>` in every response (e.g. rtsp.cpp:894-902); the client ignores it. Unknown commands → 404 (rtsp.cpp:881-883); parse errors/oversize → 400.

---

### 3. DESCRIBE response — what the server sends and what the client reads

Sunshine `cmd_describe` builds the payload with `std::endl` (LF) line endings (sun rtsp.cpp:913-994); the client only uses `strstr`/prefix scans, so line endings don't matter.

Server emits, in order:
1. `a=x-ss-general.featureFlags:<uint32>` — platform caps: bit 0x01 pen/touch, 0x02 controller touch (sun rtsp.cpp:925, values sun platform/common.h:370-384, mirrored as `LI_FF_PEN_TOUCH_EVENTS`/`LI_FF_CONTROLLER_TOUCH_EVENTS` mlc Limelight.h:1011-1012). Client → `SunshineFeatureFlags` (mlc RtspConnection.c:1144-1147); gates touch/pen input APIs only.
2. `a=x-ss-general.encryptionSupported:<uint32>` — always `SS_ENC_CONTROL_V2|SS_ENC_AUDIO` (0x01|0x04); plus `SS_ENC_VIDEO` (0x02) unless encryption mode for this address is NEVER (sun rtsp.cpp:927-945; flag values mlc Limelight-internal.h:48-50 and identically in Sunshine via the shared header).
3. `a=x-ss-general.encryptionRequested:<uint32>` — `SS_ENC_CONTROL_V2` always; `|SS_ENC_VIDEO|SS_ENC_AUDIO` when encryption is MANDATORY (sun rtsp.cpp:929-946). Client → `EncryptionFeaturesSupported`/`EncryptionFeaturesRequested` (mlc RtspConnection.c:1149-1156).
4. `a=x-nv-video[0].refPicInvalidation:1` iff the encoder probe supports RFI (sun rtsp.cpp:948-950). Client sets `ReferenceFrameInvalidationSupported` by mere substring presence of `x-nv-video[0].refPicInvalidation` (mlc RtspConnection.c:1139).
5. `sprop-parameter-sets=AAAAAU` (bare literal line, not a real fmtp) iff HEVC is not disabled (sun rtsp.cpp:952-954). Client's HEVC capability probe is `strstr(payload, "sprop-parameter-sets=AAAAAU")` — the base64 of an HEVC VPS NALU start (mlc RtspConnection.c:1104-1110).
6. `a=rtpmap:98 AV1/90000` iff AV1 is not disabled (sun rtsp.cpp:956-958). Client probes `strstr(payload, "AV1/90000")` (mlc RtspConnection.c:1090).
7. Surround params: if the HTTP launch carried `surroundParams`, that string is advertised **twice** first (sun rtsp.cpp:960-964); then for every built-in config: `a=fmtp:97 surround-params=<C><N><M><mapping digits>` where C=channelCount, N=streams, M=coupledStreams, then C single digits of channel mapping, all as bare ASCII digits concatenated (sun rtsp.cpp:966-991; 5.1/7.1 mappings pre-rotated at 977-982 to compensate GFE's mapping quirk).

Client-side parsing of the payload (`performRtspHandshake` DESCRIBE block, mlc RtspConnection.c:1084-1163):
- **Codec negotiation** (RtspConnection.c:1090-1136): AV1 if client wants AV1 and `AV1/90000` present (profile refined by `serverCodecModeSupport` SCM_* bits from the HTTP serverinfo, not from RTSP: SCM values mlc Limelight.h:506-522); else HEVC if `sprop-parameter-sets=AAAAAU` present; else H.264. Sets `NegotiatedVideoFormat` (VIDEO_FORMAT_* bits, Limelight.h:225-241).
- **Opus configs** (`parseOpusConfigurations`, RtspConnection.c:748-848): stereo is hardcoded (2ch/1stream/1coupled, map 0,1 — 757-763). For >2ch it searches the prefix `a=fmtp:97 surround-params=<channelCount>` — first match = normal quality (client then swaps GFE's `FL FR C RL RR SL SR LFE` order into `FL FR C LFE ...`, 786-799); a **second** match with the same prefix = high-quality config and sets `HighQualitySurroundSupported = true` (801-817). Missing params: hardcoded 5.1 fallback, otherwise hard failure (819-844).
- Attribute integers are parsed by `strstr(name)` then `strtoul` after the next `:` (mlc RtspConnection.c:903-941) — name match is substring-based, values are decimal (base 0, so hex would also parse).

---

### 4. The ANNOUNCE SDP (client → server)

Built by `getSdpPayloadForStreamConfig` (mlc SdpGenerator.c:567-623). Layout:

- **Header** (SdpGenerator.c:547-555):
  `v=0\r\n` `o=android 0 <rtspClientVersion> IN IPv4|IPv6 <urlSafeAddr>\r\n` `s=NVIDIA Streaming Client\r\n` — `<rtspClientVersion>`=14, address family string is literally `IPv4`/`IPv6`, `<urlSafeAddr>` is the target address ([bracketed] for v6).
- **Attributes**, each serialized as `a=<name>:<value> \r\n` — note the **trailing space before CRLF** (SdpGenerator.c:35,68). Sunshine strips exactly one trailing space from values (sun rtsp.cpp:1116-1118). Sunshine splits lines on `\r`/`\n`, reads `s=` (unused) and `a=` name:value pairs (rtsp.cpp:1082-1121); `v=`,`o=`,`t=`,`m=` lines are ignored.
- **Tail** (SdpGenerator.c:558-564): `t=0 0\r\n` `m=video <VideoPortNumber>  \r\n` (two trailing spaces; port is the server_port learned in video SETUP, 47996 hardcoded for gen<4). Sunshine ignores it.

#### Attribute inventory (emission order, Sunshine path: IS_SUNSHINE ∧ gen 7 ∧ ≥7.1.431)

Sunshine parse citations refer to `cmd_announce` (sun rtsp.cpp:1071-1311); "default" = value injected by `try_emplace` when absent (rtsp.cpp:1123-1138). All values are decimal ASCII integers unless noted.

**Sunshine-only block** (mlc SdpGenerator.c:270-313, gated on `IS_SUNSHINE()`):

| Attribute | Value | Sunshine handling |
|---|---|---|
| `x-ml-general.featureFlags` | `3` = `ML_FF_FEC_STATUS` 0x01 \| `ML_FF_SESSION_ID_V1` 0x02 (SdpGenerator.c:272-274; flags Limelight-internal.h:88-89) | parsed → `config.mlFeatureFlags` (rtsp.cpp:1155; default `0`) |
| `x-ss-general.encryptionEnabled` | `EncryptionFeaturesEnabled` bitmask: `SS_ENC_CONTROL_V2` whenever supported; `SS_ENC_VIDEO` if (supported ∧ client `ENCFLG_VIDEO`) or host requested it; `SS_ENC_AUDIO` likewise (SdpGenerator.c:276-304; ENCFLG_* Limelight.h:33-36) | parsed (rtsp.cpp:1158, default `0`); if host mode is MANDATORY and (VIDEO\|AUDIO) not both set → **403 Forbidden** (rtsp.cpp:1290-1297) |
| `x-ss-video[0].chromaSamplingType` | `1` if `NegotiatedVideoFormat & VIDEO_FORMAT_MASK_YUV444` else `0` (SdpGenerator.c:307-312) | parsed → chroma sampling (rtsp.cpp:1202, default `0`) |

**Core video block** (SdpGenerator.c:315-407):

| Attribute | Value | Sunshine handling |
|---|---|---|
| `x-nv-video[0].clientViewportWd` | `StreamConfig.width` (315-316) | parsed, **required** (rtsp.cpp:1183) |
| `x-nv-video[0].clientViewportHt` | `StreamConfig.height` (317-318) | parsed, **required** (1182) |
| `x-nv-video[0].maxFPS` | `StreamConfig.fps` (320-321) | parsed, **required** (1184) |
| `x-nv-video[0].packetSize` | `StreamConfig.packetSize`, minus `sizeof(ENC_VIDEO_HEADER)` = **32** when `SS_ENC_VIDEO` enabled (323-330; struct iv[12]+u32+tag[16], mlc Video.h:15-19; must stay a multiple of 16) | parsed, **required** (1153); server may clamp to its configured packetsize (1165-1180) |
| `x-nv-video[0].rateControlMode` | `"4"` (332) | ignored |
| `x-nv-video[0].timeoutLengthMs` | `"7000"` (334) | ignored |
| `x-nv-video[0].framesWithInvalidRefThreshold` | `"0"` (335) | ignored |
| `x-nv-video[0].initialBitrateKbps` / `initialPeakBitrateKbps` | adjustedBitrate = `bitrate*0.80` (FEC headroom), −500 if `STREAM_CFG_REMOTE`, capped 100000 (338-364) | ignored |
| `x-nv-vqos[0].bw.minimumBitrateKbps` | adjustedBitrate (366) | ignored |
| `x-nv-vqos[0].bw.maximumBitrateKbps` | adjustedBitrate (367) | parsed, **required** → `config.monitor.bitrate` (1196), but overridden when `configuredBitrateKbps` ≠ 0 (below) |
| `x-ml-video.configuredBitrateKbps` | raw `StreamConfig.bitrate` (Sunshine only, 370-373) | parsed (1205, default `0`); if nonzero Sunshine computes the real encoder bitrate itself: scales down by FEC% (if ≤80), subtracts audio bitrate (256 or 96 Kbps/ch, cap 20%), subtracts 500 Kbps overhead (cap 10%) (1250-1274) |
| `x-nv-vqos[0].fec.enable` | `"1"` (387) | ignored |
| `x-nv-vqos[0].videoQualityScoreUpdateTime` | `"5000"` (389) | ignored |
| `x-nv-vqos[0].qosTrafficType` | `"5"` if `STREAM_CFG_LOCAL` else `"0"` (400-406) | parsed (1157, default `5`) |
| `x-nv-aqos.qosTrafficType` | `"4"` if local else `"0"` (402-406) | parsed (1156, default `4`) |

**Gen5+ block** (`addGen5Options`, SdpGenerator.c:181-254; ≥7.1.431 branch is what Sunshine sees):

| Attribute | Value | Sunshine handling |
|---|---|---|
| `x-nv-general.featureFlags` | `NVFF_BASE` 0x07 \| `NVFF_RI_ENCRYPTION` 0x80 (= `135`); \| `NVFF_AUDIO_ENCRYPTION` 0x20 (= `167`) when audio encryption enabled via `ENCFLG_AUDIO` or negotiated `SS_ENC_AUDIO` — also sets client `AudioEncryptionEnabled` (188-201; constants 177-179) | parsed (default `135`); **bit 0x20 = legacy audio-encryption opt-in**, OR'd into `encryptionFlagsEnabled` as `SS_ENC_AUDIO` (rtsp.cpp:1160-1163) |
| `x-nv-general.useReliableUdp` | `"13"` (205; = encrypted control protocol request; pre-7.1.431 GFE: `"1"` + `x-nv-ri.useControlChannel:1`, 222-223) | parsed → `config.controlProtocolType` (1152, default `1`) |
| `x-nv-vqos[0].fec.minRequiredFecPackets` | `"2"` (211) | parsed (1154, default `0`) |
| `x-nv-vqos[0].bllFec.enable` | `"0"` (218) | ignored |
| `x-nv-vqos[0].fec.repairPercent` | *(pre-7.1.431 GFE only)* `"5"` for ≥3840×2160 else `"20"` (226-231) | ignored (Sunshine FEC% is server-config) |
| `x-nv-vqos[0].drc.enable` | `"0"`; (`"1"` + `x-nv-vqos[0].drc.tableType:2` only for <720×540 on GFE ≥7.1.446, 234-247) | ignored |
| `x-nv-general.enableRecoveryMode` | `"0"` (251) | ignored |

*(Gen3/Gen4 only, never sent to Sunshine: `x-nv-general.serverAddress` string / `rtsp://addr:port`; four raw-binary `x-nv-video[n].transferProtocol`/`rateControlMode` 4-byte blobs; `x-nv-vqos[0].bw.flags:14083`; `videoQosMaxConsecutiveDrops`; `averageBitrate`/`peakBitrate`; `bw.minimumBitrate`/`maximumBitrate` — SdpGenerator.c:125-175, 375-384.)*

**Gen4+ codec/audio block** (SdpGenerator.c:422-491):

| Attribute | Value | Sunshine handling |
|---|---|---|
| `x-nv-video[0].videoEncoderSlicesPerFrame` | `VideoCallbacks.capabilities >> 24`, min 1 (425-432) | parsed, **required** (1197) |
| `x-nv-vqos[0].bitStreamFormat` | **video codec selector**: `0` H.264, `1` HEVC, `2` AV1 (434-451) | parsed (1200, default `0`); `1` with HEVC disabled or `2` with AV1 disabled → **400** (1276-1288) |
| `x-nv-clientSupportHevc` | `1`/`0` alongside bitStreamFormat (438,450) | ignored |
| `x-nv-video[0].encoderFeatureSetting` | `"0"` only on GFE <7.1.408 with HEVC (441-447) | ignored |
| `x-nv-video[0].dynamicRangeMode` | `1` if `NegotiatedVideoFormat & VIDEO_FORMAT_MASK_10BIT` else `0` (456-461) | parsed → HDR (1201, default `0`) |
| `x-nv-video[0].maxNumReferenceFrames` | `0` (= host's choice) if decoder supports RFI, else `1` (467-475) | parsed, **required** (1198) |
| `x-nv-video[0].clientRefreshRateX100` | `StreamConfig.clientRefreshRateX100` (477-478) | parsed (1185, default `0`); zeroed unless within 1% of maxFPS (1189-1195) |
| `x-nv-audio.surround.numChannels` | channel count from audioConfiguration (481-482; encoding Limelight.h:208-218: `(mask<<16)\|(count<<8)\|0xCA`) | parsed, **required** (1145) |
| `x-nv-audio.surround.channelMask` | channel mask (483-484; stereo 0x3, 5.1 0x3F, 7.1 0x63F, Limelight.h:197-203) | parsed, **required** (1146) |
| `x-nv-audio.surround.enable` | `1` if >2 channels else `0` (485-490) | ignored |

**Gen7+ audio/color block** (SdpGenerator.c:493-536):

| Attribute | Value | Sunshine handling |
|---|---|---|
| `x-nv-audio.surround.AudioQuality` | `1` (high-quality surround: bitrate ≥15000, >2ch, `HighQualitySurroundSupported` from DESCRIBE, fast decoder) else `0` (494-507) | parsed, **required** → HIGH_QUALITY flag (1149-1150); for stereo it is instead derived from the `Host` header (1211-1221) |
| `x-nv-aqos.packetDuration` | `5` or `10` (ms; 10 when slow decoder or bitrate <5000 with arbitrary-duration support) (504-523) | parsed (1147, default `5`) |
| `x-nv-video[0].encoderCscMode` | `(colorSpace << 1) \| colorRange` — colorSpace 0=Rec601/1=Rec709/2=Rec2020, colorRange 0=limited/1=full (533-535; constants Limelight.h:22-30) | parsed (1199, default `0`) |

**Attributes Sunshine parses that Moonlight never emits**: `x-ss-video[0].intraRefresh` (default `0`, rtsp.cpp:1137/1203).

**riKey / riKeyId are NOT in the SDP.** They travel in the HTTPS `launch`/`resume` query (`rikey` = 16-byte AES key hex-encoded, `rikeyid` = signed decimal int; Sunshine stores the key as `gcm_key` and builds a 16-byte IV whose first 4 bytes are BE32(rikeyid), sun nvhttp.cpp:368-369, 416-419, presence enforced at 1013-1016). On the client that same key is `StreamConfig.remoteInputAesKey` / `remoteInputAesIv`, reused for encrypted RTSP (§1), the control stream, and audio/video encryption. Legacy GFE gen5's SDP had only `x-nv-ri.useControlChannel:1` (SdpGenerator.c:223), not key material.

**Error behavior**: any required attribute missing → `std::out_of_range` → **400 BAD REQUEST** (rtsp.cpp:1206-1209). On success Sunshine allocates and starts the stream session **during ANNOUNCE handling** (rtsp.cpp:1299-1310) and replies 200; PLAY is a pure formality.

---

### 5. Minimal-but-correct client SDP for Sunshine

Sunshine requires only lines matching `a=<name>:<value>`; the `v=/o=/s=/t=/m=` scaffolding is ignored (s= is captured but unused, rtsp.cpp:1103-1109). The **must-emit** set (no server default; absence = 400):

```
a=x-nv-audio.surround.numChannels:2
a=x-nv-audio.surround.channelMask:3
a=x-nv-audio.surround.AudioQuality:0
a=x-nv-video[0].packetSize:1392
a=x-nv-video[0].clientViewportWd:1920
a=x-nv-video[0].clientViewportHt:1080
a=x-nv-video[0].maxFPS:60
a=x-nv-vqos[0].bw.maximumBitrateKbps:16000
a=x-nv-video[0].videoEncoderSlicesPerFrame:1
a=x-nv-video[0].maxNumReferenceFrames:1
```
(parse sites: rtsp.cpp:1145,1146,1149-1150,1153,1182-1184,1196-1198)

Everything else defaults sanely (rtsp.cpp:1124-1138): `encoderCscMode=0`, `bitStreamFormat=0` (H.264), `dynamicRangeMode=0`, `packetDuration=5`, `useReliableUdp=1`, `minRequiredFecPackets=0`, `x-nv-general.featureFlags=135`, `x-ml-general.featureFlags=0`, `qosTrafficType` 5/4, `configuredBitrateKbps=0`, `encryptionEnabled=0`, `chromaSamplingType=0`, `intraRefresh=0`, `clientRefreshRateX100=0`.

Caveats for a minimal client:
- `x-ss-general.encryptionEnabled` defaulting to 0 makes the handshake fail with **403** if the host's encryption mode is MANDATORY (rtsp.cpp:1290-1297) — a real client should implement SS_ENC_CONTROL_V2/VIDEO/AUDIO negotiation from the DESCRIBE flags (§3 items 2-3, §4 row 2).
- `useReliableUdp` (controlProtocolType) should be `13` to get the modern encrypted control protocol (SdpGenerator.c:205).
- Omitting `x-ml-general.featureFlags` bit 0x02 (`ML_FF_SESSION_ID_V1`) means the X-SS-Ping-Payload/X-SS-Connect-Data identifiers won't be expected on the UDP/control connections; emit `3` and use them like Moonlight does.
- `bitStreamFormat` ≠ 0 must match a codec the host has enabled or you get 400 (rtsp.cpp:1276-1288).
- The whole ANNOUNCE (request line + headers + SDP) must fit Sunshine's 2048-byte receive buffer (rtsp.cpp:459).
- Values may carry one trailing space (Moonlight does); more than one is not stripped (rtsp.cpp:1116-1118).

### 6. Post-PLAY

- **No RTSP keepalive, no TEARDOWN, nothing.** After the PLAY 200, `performRtspHandshake` returns (mlc RtspConnection.c:1354-1416) and the RTSP port is never contacted again for the session; each transaction already used its own short-lived TCP connection. Sunshine registers only OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY handlers (sun rtsp.cpp:1337-1341); anything else (including TEARDOWN) gets 404 (rtsp.cpp:881-883).
- Liveness moves to the other channels: the stream session Sunshine started during ANNOUNCE waits for UDP pings (carrying the `X-SS-Ping-Payload` values, `SS_PING` = 16-byte payload + BE32 sequence, mlc Video.h:51-54) and the ENet control connection (echoing `X-SS-Connect-Data`); a client that never pings within `ping_timeout` gets the launch session discarded (sun rtsp.cpp:603-612 for pre-handshake; session teardown via `clear()`/state machine, rtsp.cpp:651-665). Client-side teardown (`LiStopConnection`) tears down the control/A-V streams without any RTSP message.
- The `Session: DEADBEEFCAFE;timeout = 90` value's `timeout` parameter is decoration; neither side implements RTSP session timeout refresh.

# Appendix C — control stream and ENet (linked)

Sources (paths into the pinned upstream checkouts named in the preamble):

- `moonlight-common-c/src/ControlStream.c`, `InputStream.c`, `Input.h`, `Video.h`,
  `Limelight-internal.h`, `Limelight.h`, `SdpGenerator.c`, `RtspConnection.c`, `Misc.c`,
  `PlatformCrypto.c` — Moonlight client (the behavior we reimplement).
- `moonlight-common-c/enet/` — bundled ENet fork `cgutman/enet` @ `aca8784`
  (`moonlight-common-c/.gitmodules`), based on ENet **1.3.17** (`enet/include/enet/enet.h:26-28`).
- `Sunshine/src/stream.cpp`, `network.cpp`, `rtsp.cpp`, `config.cpp` — server side.

Scope: modern-protocol only (server appversion quad `[0] >= 7`, i.e. Sunshine, which advertises
`7.1.431.-1`; `IS_SUNSHINE()` = `AppVersionQuad[3] < 0`, `Limelight-internal.h:85`). Legacy
TCP-47995 (gen 3/4) and gen-5 plaintext paths are noted only where they explain a constant.

---

### 1. ENet usage (client side)

**Port.** Negotiated by RTSP `SETUP streamid=control/13/0`: client parses `server_port` from the
`Transport:` header, falling back to **47999** if parsing fails
(`RtspConnection.c:1318-1330`). Sunshine binds control at `config::sunshine.port + 10`
(`stream.h:20` `CONTROL_PORT = 10`, `network.cpp:224-232` `map_port`; default base 47989 →
47999). Transport is UDP (ENet).

**Connect-data.** The RTSP SETUP control response may carry `X-SS-Connect-Data: <u32>`; the
client stores it in `ControlConnectData` (`RtspConnection.c:1309-1316`) and passes it as the
`data` field of the ENet CONNECT command (`ControlStream.c:1788`). Sunshine matches the
incoming ENet peer to the launched session by this value when the client advertised
`ML_FF_SESSION_ID_V1` (0x02) in `x-ml-general.featureFlags` (falls back to source-IP matching
otherwise) — `stream.cpp:620-655`, `Limelight-internal.h:88-89`.

**Host/peer creation** (`ControlStream.c:1750-1838`):

- `enet_host_create(family, localAddr-or-NULL, /*peers*/1, /*channelLimit*/CTRL_CHANNEL_COUNT, 0, 0)`
  — 1 peer, **0x30 (48) channels**, unlimited in/out bandwidth (`:1771-1773`).
- QoS/DSCP tagging enabled on the socket before connecting: `ENET_SOCKOPT_QOS = 1` (`:1785`).
  (The fork auto-disables QoS if 3 connect attempts in a row go unanswered, `protocol.c:1736-1740`.)
- `enet_host_connect(client, &remote, CTRL_CHANNEL_COUNT, ControlConnectData)` (`:1788`).
- Connect must complete within **10 s** (`CONTROL_STREAM_TIMEOUT_SEC`, `:144,1797`).
- After the CONNECT event: `enet_host_flush()` so the verify-connect ACK goes out immediately
  (`:1829`), then `enet_peer_timeout(peer, /*limit*/2, /*min*/10000, /*max*/10000)` — i.e. the
  peer is declared dead after retransmission backoff exceeds 2× while ≥10 s have elapsed, hard
  cap 10 s (`:1836-1838`).
- Ping interval: default `ENET_PEER_PING_INTERVAL = 500 ms` (`enet.h:232`). The client's receive
  thread wakes early enough to send pings when the connection is otherwise idle
  (`ControlStream.c:1112-1152`).
- Sunshine's host: `enet_host_create(af, addr, 128 peers, 0 → max channelLimit, 0, 0)`,
  QoS on, no checksum, no compressor (`network.cpp:190-206`).

**Channel IDs** (`Limelight-internal.h:56-66`) — one send stream per input class so HOL
blocking is contained:

| id | name |
|----|------|
| 0x00 | `CTRL_CHANNEL_GENERIC` (Start A/B, ping, FEC status, haptics-enable; also everything server→client — Sunshine always sends on channel 0, `stream.cpp:420-422`) |
| 0x01 | `CTRL_CHANNEL_URGENT` (IDR request, RFI, LTR ACK) |
| 0x02 | `CTRL_CHANNEL_KEYBOARD` |
| 0x03 | `CTRL_CHANNEL_MOUSE` (move, buttons, scroll, hscroll) |
| 0x04 | `CTRL_CHANNEL_PEN` |
| 0x05 | `CTRL_CHANNEL_TOUCH` |
| 0x06 | `CTRL_CHANNEL_UTF8` |
| 0x10+n | `CTRL_CHANNEL_GAMEPAD_BASE` (n = controller index, 0-15) |
| 0x20+n | `CTRL_CHANNEL_SENSOR_BASE` (n = controller index) |

If a channel id ≥ peer channel count, or the peer is GFE, everything goes to channel 0
(`ControlStream.c:763-766`).

**Reliability per message** — see the tables in §3/§5. Summary: everything is
`ENET_PACKET_FLAG_RELIABLE` except (a) frame-FEC-status, sent `ENET_PACKET_FLAG_UNSEQUENCED`
(`ControlStream.c:1408-1413`), (b) touch/pen/controller-touch HOVER/MOVE events and controller
motion events, sent flags=0 (plain unreliable-sequenced) (`InputStream.c:1349,1398,1500,529-534`).
Against GFE (non-Sunshine) the client forces everything reliable (`ControlStream.c:698-701`).

**Send pacing.** After queueing a packet the client calls `enet_host_service(client, NULL, 0)`
and, for reliable packets, busy-waits up to 10×1 ms until the packet is on the wire
(`ControlStream.c:773-799`). Not wire-visible; a Rust client just needs to flush promptly.

---

### 2. ENet wire subset to implement (bundled fork = stock ENet 1.3 wire format)

**Verdict on fork deviations:** the cgutman fork does **not** change the ENet 1.3 wire format.
`protocol.h` is byte-identical in layout to stock 1.3.17: 12-bit peer IDs, 2 session bits, same
command set, same constants (`ENET_PROTOCOL_MAXIMUM_PEER_ID = 0xFFF`, `protocol.h:23`;
session mask `3 << 12`, flags bits 14/15, `protocol.h:47-58`). No enet6/"new protocol" packet,
no extended peer IDs, no random session bits beyond stock, no protocol version constant on the
wire. What the fork changes is host-local:

- `ENetAddress` is `{socklen_t addressLength; struct sockaddr_storage address;}` for dual-stack
  IPv4/IPv6 (`enet.h:80-84`); `enet_host_create()` takes an address-family first argument
  (`host.c:30`). Wire-invisible.
- `ENET_HOST_DEFAULT_MTU = 900` (stock: 1400) — `enet.h:215`. This is the MTU advertised in
  CONNECT.
- `ENET_PROTOCOL_MAXIMUM_MTU = 4096` (1400 on WiiU/3DS) — `protocol.h:12-17`.
- RTO formula: `rto = rtt + min(rtt, 4 * max(1, rttVariance))`, capped at `timeoutMaximum / 5`
  (`protocol.c:1507-1510`) — retransmit-timing only.
- QoS auto-disable during connect (`protocol.c:1736-1740`).

**Checksum and compression are OFF.** Neither Moonlight nor Sunshine sets `host->checksum` or a
compressor, so the optional 4-byte checksum field is never present and
`ENET_PROTOCOL_HEADER_FLAG_COMPRESSED` is never set (`protocol.c:1032-1033,1071-1084,1711-1717`;
`network.cpp:190-206`). A minimal client may hard-fail on the COMPRESSED flag.

All multi-byte ENet protocol fields are **big-endian** (network order).

#### 2.1 UDP datagram header (`ENetProtocolHeader`, `protocol.h:69-73`)

```
offset size  field
0      2     peerID (BE)  bits 0-11: peer id
                          bits 12-13: sender's outgoingSessionID
                          bit 14: compressed (never set here)
                          bit 15: sentTime field present
2      2     sentTime (BE, low 16 bits of sender ms clock) — ONLY if bit 15 set
```

Header is 2 bytes when no sentTime, 4 with it (`protocol.c:1031,1680-1687`). The sentTime flag
is set whenever the datagram carries any acknowledge-flagged (reliable) command
(`protocol.c:1520`). During connection (before the peer has a remote peer ID) the client sends
`peerID = 0xFFF` with session bits 0: session bits are only ORed in when
`outgoingPeerID < ENET_PROTOCOL_MAXIMUM_PEER_ID` (`protocol.c:1708-1710`). Receivers use
`peerID == 0xFFF` to mean "connecting peer, match by address" (`protocol.c:1035-1043`).

After the header, up to `ENET_PROTOCOL_MAXIMUM_PACKET_COMMANDS = 32` commands are concatenated.
Each starts with (`protocol.h:75-80`):

```
0  1  command    bits 0-3: command number (ENET_PROTOCOL_COMMAND_*)
                 bit 6: FLAG_UNSEQUENCED
                 bit 7: FLAG_ACKNOWLEDGE (receiver must ack this command)
1  1  channelID  (0xFF for connection-level commands)
2  2  reliableSequenceNumber (BE)
```

#### 2.2 Commands a minimal client needs

Command numbers (`protocol.h:27-45`): ACKNOWLEDGE=1, CONNECT=2, VERIFY_CONNECT=3, DISCONNECT=4,
PING=5, SEND_RELIABLE=6, SEND_UNRELIABLE=7, SEND_FRAGMENT=8, SEND_UNSEQUENCED=9,
BANDWIDTH_LIMIT=10, THROTTLE_CONFIGURE=11, SEND_UNRELIABLE_FRAGMENT=12.

**CONNECT** (client→server, `command = 2 | FLAG_ACKNOWLEDGE`, channelID 0xFF,
reliableSequenceNumber = 1st outgoing seq on channel 0xFF; layout `protocol.h:89-105`, values
from `host.c:186-266` with Moonlight's parameters):

```
hdr(4) +
2   outgoingPeerID        = client's own (incoming) peer index = 0 (single-peer host)
1   incomingSessionID     = 0 (initial)
1   outgoingSessionID     = 0 (initial)
4   mtu                   = 900            (host.c:216 <- enet.h:215)
4   windowSize            = 65536          (outgoingBandwidth==0 -> MAX, host.c:218-229)
4   channelCount          = 0x30 (48)
4   incomingBandwidth     = 0
4   outgoingBandwidth     = 0
4   packetThrottleInterval     = 5000
4   packetThrottleAcceleration = 2
4   packetThrottleDeceleration = 2
4   connectID             = random nonzero (echoed in VERIFY_CONNECT; also would seed checksum)
4   data                  = ControlConnectData (X-SS-Connect-Data, else 0)
```

**VERIFY_CONNECT** (server→client, same layout minus `data`, `protocol.h:107-122`). Client
validation (`protocol.c:949-1010`): reject unless `packetThrottle*` echo ours and
`connectID` matches. Adopt: `outgoingPeerID` (use as `peerID` in all subsequent headers),
`incomingSessionID`/`outgoingSessionID` (server may bump them; use as our outgoing session
bits), clamp channelCount to what we asked, `mtu` (clamped to [576,4096]; Sunshine will answer
with its own 900), `windowSize` (min of ours and theirs). VERIFY_CONNECT itself carries
FLAG_ACKNOWLEDGE and must be acked — enet does this automatically before raising the CONNECT
event; Moonlight then flushes so the ack isn't delayed (`ControlStream.c:1828-1829`).

**ACKNOWLEDGE** (`protocol.h:82-87`, sent WITHOUT flag bits):

```
hdr(4: command=1, channelID = acked command's channel,
       reliableSequenceNumber = current incoming reliable seq of that channel) +
2   receivedReliableSequenceNumber  (the acked command's reliableSequenceNumber, BE)
2   receivedSentTime                (echo of the datagram header's sentTime)
```

Rule: queue an ack for **every received command with FLAG_ACKNOWLEDGE**, but only when the
datagram header carried a sentTime (bit 15) (`protocol.c:1189-1215`). While in
ACKNOWLEDGING_DISCONNECT state, only DISCONNECT commands are acked. Acks are batched into the
next outgoing datagram for that peer (sent first, `protocol.c:1631-1632`).

**PING** (`protocol.h:145-148`): just the 4-byte command header, `command = 5 | FLAG_ACKNOWLEDGE`,
channelID 0xFF, consumes a reliable sequence number on channel 0xFF. Sent when a peer has been
idle ≥ pingInterval (500 ms) and nothing reliable is in flight (`protocol.c:1645-1654`).
Received pings need nothing beyond the ack.

**SEND_RELIABLE** (`protocol.h:150-154`): `command = 6 | FLAG_ACKNOWLEDGE`, channelID = app
channel, per-channel incrementing reliableSequenceNumber (starting at 1), then
`u16 dataLength (BE)` + payload.

**SEND_UNRELIABLE** (`protocol.h:156-161`): `command = 7` (no flags), header's
reliableSequenceNumber = channel's current **outgoing reliable** seq (the "anchor"), then
`u16 unreliableSequenceNumber (BE)` (per-channel counter reset whenever a new reliable seq is
issued) + `u16 dataLength` + payload. Receiver drops it if its unreliable seq is stale within
the current reliable anchor.

**SEND_UNSEQUENCED** (`protocol.h:163-168`): `command = 9 | FLAG_UNSEQUENCED`, then
`u16 unsequencedGroup (BE)` (global per-peer counter) + `u16 dataLength` + payload. Receiver
dedups via a 1024-entry window bitmap (`ENET_PEER_UNSEQUENCED_WINDOW_SIZE`, `enet.h:233-235`).
Used only for `SS_FRAME_FEC_STATUS`.

**DISCONNECT** (`protocol.h:139-143`): 4-byte header + `u32 data`. Graceful =
`command = 4 | FLAG_ACKNOWLEDGE` when the peer is CONNECTED (enet_peer_disconnect_later path);
abortive `enet_peer_disconnect_now` sends it **unsequenced** (`4 | FLAG_UNSEQUENCED`) and
resets locally without waiting (`peer.c` disconnect functions). Moonlight always uses
`data = 0`.

**BANDWIDTH_LIMIT / THROTTLE_CONFIGURE** (`protocol.h:124-137`): both peers use bandwidth 0 and
default throttle, so these are never sent by Moonlight or Sunshine; a client only needs to
*accept* them (ack if flagged) and may ignore the contents.

#### 2.3 Sequencing / windows / retransmission

- Reliable seq numbers are per-channel u16, starting at 1 (channel 0xFF for connection-level
  commands). Window bookkeeping: 16 windows × 0x1000 (`ENET_PEER_RELIABLE_WINDOWS(_SIZE)`,
  `enet.h:238-240`); a sender must not have a full window outstanding (~4096 unacked in one
  window blocks, `protocol.c` check_outgoing / `peer.c` queue_outgoing). With control-stream
  message rates this is unreachable, but wrap-around handling (u16 arithmetic, "freeing" old
  windows on ack) must be correct.
- On ack receipt, remove the command from the sent-reliable list; RTT is computed from
  `receivedSentTime` (that's why the periodic 0x0200 message is sent reliable —
  `ControlStream.c:1424-1429`).
- Retransmit a sent reliable command when its RTO expires
  (`rtt + min(rtt, 4*max(1,rttVar))`, doubling per attempt, cap `timeoutMaximum/5` = 2 s with
  Moonlight's 10 s peer timeout, `protocol.c:1444-1476,1507-1510`). Peer is declared
  disconnected (event) when retries exceed `timeoutLimit`×base and ≥`timeoutMinimum` elapsed,
  or unconditionally after `timeoutMaximum` (10 s here).

#### 2.4 MTU / fragmentation

Fragmentation threshold: `fragmentLength = peer->mtu - sizeof(ENetProtocolHeader)(4)
- sizeof(ENetProtocolSendFragment)(24)` = **872 bytes** at mtu 900 (`peer.c:119-123`; minus 4
more only if checksums were on). Packets larger than that go out as SEND_FRAGMENT chains
(startSequenceNumber/fragmentCount/fragmentNumber/totalLength/fragmentOffset, all BE,
`protocol.h:170-179`).

**Control messages never fragment in practice**: the largest client→server message is an input
packet (≤ `MAX_INPUT_PACKET_SIZE` = 128, `InputStream.c:28`) plus ≤ 28 bytes of envelope; the
largest server→client is adaptive-triggers (~35 bytes plaintext + 28 envelope). A Rust client
can therefore skip *sending* fragments entirely, and receiving them can be a hard error (or
minimal support for robustness). It must still parse SEND_FRAGMENT far enough to ack it if it
ever appears.

---

### 3. Control message layer (NVCTL, inside ENet packet payloads)

Every ENet packet payload on the control connection is one control message. Framings
(`ControlStream.c:11-32`):

- **Plaintext ENet framing (V1)** — used only on non-encrypted streams (GFE 7 < 7.1.431):
  `u16 LE type` + raw payload (no length; length = ENet packet length − 2).
- **Encrypted stream** (always, vs Sunshine): outer envelope `NVCTL_ENCRYPTED_PACKET_HEADER`
  (§4) whose plaintext is a **V2 header**: `u16 LE type` + `u16 LE payloadLength` + payload.
- (Legacy gen3/4 TCP framing `u16 LE type` + `u16 LE payloadLength` exists on port 47995 only.)

#### 3.1 Type codes (gen-7 encrypted a.k.a. Sunshine dialect)

Client tables `ControlStream.c:203-217` (`packetTypesGen7Enc`), server table
`stream.cpp:53-70`. All type values are little-endian u16 on the wire.

| type | dir | meaning | reliability/channel (client) |
|------|-----|---------|------------------------------|
| 0x0305 | C→S | Start A | reliable, ch 0 (`ControlStream.c:1870-1875`) |
| 0x0307 | C→S | Start B | reliable, ch 0 (`:1904-1909`) |
| 0x0302 | C→S | Request IDR frame | reliable, ch 1 (`:1523-1528`) |
| 0x0301 | C→S | Invalidate reference frames (RFI) | reliable, ch 1 (`:1548-1553`) |
| 0x0350 | C→S | LTR frame ACK (Sunshine ext, `SS_LTR_FRAME_ACK_PTYPE`, `Video.h:73`) | reliable, ch 1 (`:1570-1575`) |
| 0x0200 | C→S | Periodic ping | reliable, ch 0, every 100 ms (`:1430-1435`) |
| 0x0201 | C→S | Loss stats (legacy, GFE < 7.1.415 only) | flags 0, ch 0, every 50 ms (`:1470-1475`) |
| 0x5502 | C→S | Frame FEC status (`SS_FRAME_FEC_PTYPE`, `Video.h:57`) | **unsequenced**, ch 0 (`:1408-1413`) |
| 0x0206 | C→S | Input data (all §5 packets) | per-input, per-channel |
| 0x0204 | C→S | Frame stats (unused — never sent) | — |
| 0x010b | S→C | Rumble | (server: reliable ch 0) |
| 0x0109 | S→C | Termination (extended, u32 BE code) | |
| 0x010e | S→C | HDR mode + metadata | |
| 0x5500 | S→C | Rumble triggers (Sunshine ext) | |
| 0x5501 | S→C | Set motion event state (Sunshine ext) | |
| 0x5502 | S→C | Set RGB LED (Sunshine ext) | |
| 0x5503 | S→C | DualSense adaptive triggers (Sunshine ext) | |
| 0x0001 | both | Encrypted envelope marker (§4) | |

Note the deliberate collision: **0x5502 is frame-FEC-status client→server and RGB-LED
server→client** — type meaning is directional.

(For reference, the plaintext gen-7 dialect differs: termination is 0x0100 with a u16 LE
reason, and there is no IDR-request type — the client substitutes an RFI. `ControlStream.c:189-202`.)

#### 3.2 Payloads the client sends

All payload integers little-endian unless stated.

- **Start A** (0x0305): 2 bytes, both zero (`startAGen5`, `ControlStream.c:225`). Sunshine only
  logs it (`stream.cpp:1114-1116`); no reply. (GFE replied; the client fire-and-forgets over
  ENet either way — `sendMessageAndDiscardReply` only reads a reply on the TCP path,
  `ControlStream.c:861-884`.)
- **Start B** (0x0307): 1 byte, zero (`startBGen5`, `:226`). No reply.
- **Request IDR** (0x0302): 2 bytes, both zero (`requestIdrFrameGen7Enc`, `:228`). Sunshine
  raises an IDR event → next frame is an IDR (`stream.cpp:1138-1142`).
- **RFI** (0x0301): 24 bytes = `SS_RFI_REQUEST { u32 firstFrameIndex; u32 reserved1;
  u32 lastFrameIndex; u32 reserved2[3]; }` (`Video.h:80-86`, sent `ControlStream.c:1542-1553`
  with reserved = 0). Sunshine reads it as two LE i64s: `firstFrame = bytes 0-7`,
  `lastFrame = bytes 8-15` (`stream.cpp:1144-1155`) — i.e. effectively `u64 first, u64 last,
  u64 zero`. Encoder responds by referencing only frames outside [first,last] or producing an
  IDR.
- **LTR ACK** (0x0350): 8 bytes `SS_LTR_FRAME_ACK { u32 frameIndex; u32 reserved=0; }`
  (`Video.h:74-77`). Sent when a received frame is marked long-term-reference and RFI is
  enabled (`ControlStream.c:431-453,1562-1580`). (Current `stream.cpp` has no handler mapped —
  it lands in the "Unknown type" debug path; harmless to send, required only for LTR-based RFI.)
- **Periodic ping** (0x0200): 8-byte payload; client writes `u16 LE 0x0004` ("length of
  payload") then `u32 LE 0` (timestamp), last 2 bytes unspecified/zero
  (`ControlStream.c:1391-1396`). Sent every `PERIODIC_PING_INTERVAL_MS = 100` (`:298,1442`).
  Sunshine ignores the body but every received control message (this one included) resets the
  session's ping timeout (`stream.cpp:723`, default `ping_timeout` **10 s**, `config.cpp:808`).
  **This is the session keep-alive: stop sending it and Sunshine kills the session.**
- **Legacy loss stats** (0x0201, only for GFE 7.0.x < 7.1.415; Sunshine never sees it): 32
  bytes LE: `u32 0` (loss count), `u32 50` (report interval ms), `u32 1000`, `u64
  lastGoodFrame`, `u32 0`, `u32 0`, `u32 0x14` (`ControlStream.c:1459-1467`), every 50 ms.
- **Frame FEC status** (0x5502): 19 bytes, **fields big-endian** (`Video.h:56-70`, filled at
  `RtpVideoQueue.c:94-108`):

  ```
  u32 frameIndex; u16 highestReceivedSequenceNumber; u16 nextContiguousSequenceNumber;
  u16 missingPacketsBeforeHighestReceived; u16 totalDataPackets; u16 totalParityPackets;
  u16 receivedDataPackets; u16 receivedParityPackets;
  u8 fecPercentage; u8 multiFecBlockIndex; u8 multiFecBlockCount;
  ```

  Best-effort telemetry, only sent if the client advertised `ML_FF_FEC_STATUS` (0x01). Optional
  for a minimal client.
- **Input data** (0x0206): see §5.

#### 3.3 Payloads the client receives (Sunshine, after decryption; V2 header then payload)

Parsing in `queueAsyncCallback` (`ControlStream.c:1032-1102`) and structs in
`stream.cpp:186-270`. All LE except noted. (Offsets below are after the 4-byte V2 header.)

- **Rumble 0x010b**: `u32 reserved ("useless", Sunshine sends 0xC0FFEE)` + `u16 controller` +
  `u16 lowFreq` + `u16 highFreq` (client skips the first 4 bytes, `ControlStream.c:1046-1053`).
- **Rumble triggers 0x5500**: `u16 controller, u16 leftTriggerMotor, u16 rightTriggerMotor`.
- **Set motion event 0x5501**: `u16 controller, u16 reportRateHz, u8 motionType`
  (1=accel, 2=gyro). Client should begin/stop streaming 0x55000006 motion input accordingly.
- **Set RGB LED 0x5502**: `u16 controller, u8 r, u8 g, u8 b`.
- **Adaptive triggers 0x5503**: `u16 controller, u8 eventFlags (0x04=right,0x08=left),
  u8 typeLeft, u8 typeRight, u8 left[10], u8 right[10]` (`DS_EFFECT_PAYLOAD_SIZE = 10`,
  `Limelight.h:478`).
- **HDR mode 0x010e**: `u8 enabled` + (Sunshine ext) `SS_HDR_METADATA` = 11 × u16 LE:
  displayPrimaries[3]{x,y} (normalized/50000), whitePoint{x,y}, maxDisplayLuminance (nits),
  minDisplayLuminance (0.0001 nit), maxContentLightLevel, maxFrameAverageLightLevel,
  maxFullFrameLuminance (`Limelight.h:975-996`, parsed `ControlStream.c:1267-1293`).
- **Termination 0x0109**: `u32 error code, BIG-endian` (`stream.cpp:189-193,1331-1337`;
  parsed BE at `ControlStream.c:1305-1310`). Known codes normalized by the client
  (`:1313-1333`): `0x80030023` graceful stop (`ML_ERROR_GRACEFUL_TERMINATION` if any frame was
  seen, else "unexpected early termination"), `0x800e9403` frame-conversion failure,
  `0x800e9302` protected content. Anything < 6 total bytes is the legacy short form:
  `u16 LE reason`, `0x0100` = intended termination (`:1336-1359`).

---

### 4. Control encryption (the 0x0001 envelope)

**When.** `encryptedControlStream = APP_VERSION_AT_LEAST(7,1,431)` (`ControlStream.c:309`) —
always true vs Sunshine (advertises 7.1.431). Client also emits
`a=x-nv-general.useReliableUdp:13` in the RTSP ANNOUNCE SDP (`SdpGenerator.c:205`); Sunshine
stores that as `controlProtocolType == 13` and then **drops any packet whose first u16 is not
0x0001** (`stream.cpp:692-695`), and the client likewise asserts/drops non-0x0001 inbound
(`ControlStream.c:1219-1252`). So on this dialect **every control message in both directions is
encrypted — Start A/B, pings, input, everything**. There is no plaintext type on the wire
except the envelope marker itself.

**Key.** AES-128-GCM with the 16-byte session key `StreamConfig.remoteInputAesKey` — the
`riKey` exchanged in the HTTPS `/launch`/`/resume` request (Sunshine: `launch_session.gcm_key`,
`stream.cpp:2268-2271`). No AAD; 16-byte tag (`PlatformCrypto.c:142ff`, mbedtls/openssl GCM).

**Envelope layout** (`NVCTL_ENCRYPTED_PACKET_HEADER`, `ControlStream.c:25-32`; server mirror
`stream.cpp:275-292`):

```
offset size  field
0      2     encryptedHeaderType = 0x0001 (LE)
2      2     length (LE) = 4 (seq) + 16 (tag) + 4 (V2 hdr) + payloadLen
4      4     seq (LE)  — per-direction monotonic counter starting at 0
8      16    AES-GCM tag
24     N     ciphertext = ENCRYPT( u16 LE type | u16 LE payloadLength | payload )
```

Total ENet payload = 8 + 16 + 4 + payloadLen. (Client build: `ControlStream.c:703-737`; tag is
written immediately after the 8-byte header, ciphertext after the tag, `:585-590`. Runt check on
receive: `length ≥ 4+16+4`, `:611-615`; Sunshine's check `length ≥ 16+4+4`, `stream.cpp:1191-1194`.)

**IV construction** — two schemes, chosen by the negotiated `SS_ENC_CONTROL_V2` (0x01) bit:

- **V2** (Sunshine ≥ v0.20-era; negotiated via SDP: server advertises
  `x-ss-general.encryptionSupported`, client echoes `x-ss-general.encryptionEnabled` — client
  always enables CONTROL_V2 when supported, `SdpGenerator.c:276-304`, `RtspConnection.c:1145-1156`,
  `rtsp.cpp:1158`): 12-byte IV. Bytes 0-3 = seq in LE; bytes 4-9 = 0; byte 10 = direction tag;
  byte 11 = `'C'` (0x43). Direction byte: `'C'` (0x43) for client-originated, `'H'` (0x48) for
  host-originated (`ControlStream.c:553-567` encrypt / `:617-630` decrypt;
  `stream.cpp:566-580,1199-1218`). So client-sent IV =
  `[seq0,seq1,seq2,seq3,0,0,0,0,0,0,'C','C']`, host-sent IV = `[... 'H','C']`.
- **Legacy** (bit clear, matches Nvidia): 16-byte IV, all zero except `iv[0] = (u8)seq`
  (`ControlStream.c:568-574`; `stream.cpp:580-585`). Yes, it repeats after 256 messages;
  mimicry is mandatory only against hosts without CONTROL_V2.

**Sequence counters** are independent per direction (client: `currentEnetSequenceNumber`,
`ControlStream.c:114,722`; server: `session->control.seq`, `stream.cpp:563`). Receivers accept
any seq value (no replay window — "seq is accepted as an arbitrary value in Moonlight",
`stream.cpp:279`); the seq exists to build the IV. **Never reuse a seq for a given
direction/key** or GCM breaks.

**V2→V1 conversion note:** after decrypting, Moonlight collapses the inner V2 header to V1 by
deleting the u16 length (`ControlStream.c:657-660`); i.e. the inner `payloadLength` is
redundant with the envelope length and is not otherwise validated by the client. Sunshine skips
4 bytes (full V2 header) of the plaintext (`stream.cpp:1231-1232`).

---

### 5. Input packets (type 0x0206 payload)

On the encrypted control stream, the 0x0206 payload is the **plaintext** input packet — the
outer GCM envelope is the only crypto (`InputStream.c:238-252`). (Legacy non-encrypted gen 5/7
instead wraps each input packet as `u32 BE ciphertextLen | AES-GCM tag(16) | ciphertext`, with a
bizarre rolling IV: initial IV = riKeyIv from launch, and after any packet whose ciphertext ≥
32 bytes, the next IV = last 16 bytes of that ciphertext (`InputStream.c:160-193,283-291`;
Sunshine's mirror `stream.cpp:1157-1181`). Not needed for a Sunshine-dialect client, since
`controlProtocolType == 13` forces whole-stream encryption; Sunshine's 0x0001 handler passes
input plaintext straight through, `stream.cpp:1240-1245`.)

**Common header** (`NV_INPUT_HEADER`, `Input.h:11-14`), packed:

```
0  4  size  (BIG-endian)  = total packet size EXCLUDING this field (i.e. 4 (magic) + body)
4  4  magic (LITTLE-endian) = packet type
```

Packet bodies (`Input.h`, `#pragma pack(1)`; magic values are the gen-5+ ones; `netfloat` =
4-byte little-endian IEEE float, `Input.h:9`, `InputStream.c:309-320`):

| magic (LE) | packet | body after header (offsets from byte 8) | channel | flags |
|------------|--------|-------------------------------------------|---------|-------|
| 0x00000003 / 0x00000004 | key down / key up (`NV_KEYBOARD_PACKET`) | `u8 flags (Sunshine ext: bit0 NONNORMALIZED?; 0 for GFE)`, `i16 LE keyCode (Win VK)`, `u8 modifiers`, `u16 zero` (`Input.h:22-30`, filled `InputStream.c:946-951`) | 0x02 | REL |
| 0x00000017 | UTF-8 text (`NV_UNICODE_PACKET`) | UTF-8 bytes; header.size = 4 + textLen; **client splits to one code point per packet** (`InputStream.c:547-605,984-986`) | 0x06 | REL |
| 0x00000007 | mouse move relative (`MOUSE_MOVE_REL_MAGIC_GEN5`) | `i16 BE deltaX`, `i16 BE deltaY` (`Input.h:39-45`; BE16 at `InputStream.c:383-410`) | 0x03 | REL |
| 0x00000005 | mouse move absolute | `i16 BE x`, `i16 BE y`, `i16 unused(0)`, `i16 BE width-1`, `i16 BE height-1` (reference dims; the −1 is a GFE rounding workaround kept for Sunshine too, `InputStream.c:450-459`) | 0x03 | REL |
| 0x00000008 / 0x00000009 | mouse button down / up (gen5+) | `u8 button` (1=left,2=middle,3=right,4=X1,5=X2) (`Input.h:62-67`, `InputStream.c:867-873`) | 0x03 | REL |
| 0x0000000A | vertical scroll (gen5+, `SCROLL_MAGIC_GEN5`) | `i16 BE scrollAmt1`, `i16 BE scrollAmt2 (=amt1)`, `i16 zero` — amount in 1/120 wheel-delta units (`Input.h:111-118`, `InputStream.c:1222-1231`) | 0x03 | REL |
| 0x55000001 | horizontal scroll (Sunshine) | `i16 BE scrollAmount` (`Input.h:120-124`, `InputStream.c:1308-1310`) | 0x03 | REL |
| 0x0000000C | multi-controller state (gen5+) | `i16 LE 0x001A (headerB)`, `i16 LE controllerNumber`, `i16 LE activeGamepadMask`, `i16 LE 0x0014 (midB)`, `i16 LE buttonFlags(low16)`, `u8 leftTrigger`, `u8 rightTrigger`, `i16 LE lsX, lsY, rsX, rsY`, `i16 LE 0x009C (tailA)`, `i16 LE buttonFlags2(high16, Sunshine ext; 0 for GFE)`, `i16 LE 0x0055 (tailB)` (`Input.h:87-109`, `InputStream.c:1104-1128`) | 0x10+n | REL |
| 0x0000000D | enable haptics (`ENABLE_HAPTICS_MAGIC`) | `u16 LE enable(1)` — GFE won't send rumble without it; harmless for Sunshine (`Input.h:16-20`, `InputStream.c:619-648`) | 0x00 | REL |
| 0x55000002 | touch (`SS_TOUCH_PACKET`) | `u8 eventType`, `u8 zero`, `u16 LE rotation (0-359 or 0xFFFF unknown)`, `u32 LE pointerId`, `netfloat x`, `y` (normalized 0-1), `pressureOrDistance`, `contactAreaMajor`, `contactAreaMinor` (`Input.h:126-138`, `InputStream.c:1345-1361`) | 0x05 | REL unless eventType ∈ {HOVER(?),MOVE} → flags 0 |
| 0x55000003 | pen (`SS_PEN_PACKET`) | `u8 eventType`, `u8 toolType`, `u8 penButtons`, `u8 zero`, `netfloat x`, `y`, `pressureOrDistance`, `u16 LE rotation`, `u8 tilt`, `u8 zero`, `netfloat contactAreaMajor`, `contactAreaMinor` (`Input.h:140-155`, `InputStream.c:1394-1414`) | 0x04 | REL unless MOVE/HOVER and buttons unchanged |
| 0x55000004 | controller arrival | `u8 controllerNumber`, `u8 type`, `u16 LE capabilities`, `u32 LE supportedButtonFlags` (`Input.h:157-164`); follow with a multi-controller packet (`InputStream.c:1444-1471`) | 0x10+n | REL |
| 0x55000005 | controller touch | `u8 controllerNumber`, `u8 eventType`, `u8 zero`, `u8 touchpadIndex`, `u32 LE pointerId`, `netfloat x`, `y`, `pressure` (`Input.h:166-177`) | 0x10+n | REL unless MOVE/HOVER |
| 0x55000006 | controller motion | `u8 controllerNumber`, `u8 motionType (1=accel m/s², 2=gyro deg/s)`, `u8 zero[2]`, `netfloat x`, `y`, `z` (`Input.h:179-188`) | 0x20+n | flags 0; **reliable** iff gyro (0,0,0) so the null state always lands (`InputStream.c:529-534`) |
| 0x55000007 | controller battery | `u8 controllerNumber`, `u8 batteryState`, `u8 batteryPercentage`, `u8 zero` (`Input.h:190-197`) | 0x10+n | REL |

Touch/pen event availability is gated on Sunshine's `x-ss-general.featureFlags` SDP attribute
(`LI_FF_PEN_TOUCH_EVENTS`, `InputStream.c:1336,1385`; controller touch/motion on
`LI_FF_CONTROLLER_TOUCH_EVENTS`, `:1483,1542`).

Client-side batching (wire-relevant only in that packets may be coalesced/replaced before
send): relative deltas accumulate and are chunked to i16 range; absolute moves send latest
only; mouse/pen events are held ~1 ms to batch (`InputStream.c:43-47,341-509`).

**Periodic obligations recap:** the only mandatory keep-alive on the control stream is the
encrypted 0x0200 ping every **100 ms** (Sunshine drops the session after `ping_timeout` =
10 s without *any* control message, `stream.cpp:1273-1277`, `config.cpp:808`). Additionally
ENet-level pings (500 ms) keep the transport itself alive/measured, and the **video and audio
UDP sockets** need their own ping: send the 16-byte ASCII `X-SS-Ping-Payload` from the
corresponding RTSP SETUP response with a trailing `u32 BE sequenceNumber` (SS_PING,
`Video.h:50-54`) every 500 ms from the local port you want media delivered to
(`VideoStream.c:66-81`; legacy hosts get the literal `"PING"`), or Sunshine never learns where
to send A/V (`send_feedback_msg` "still waiting for PING", `stream.cpp:964-967`).

---

### 6. Session lifecycle over the control connection

Order (client, `Connection.c:441-560`): RTSP handshake (DESCRIBE → SETUP audio/video/control →
ANNOUNCE → PLAY) → **control stream start** → video → audio → input.

1. **Start**: ENet connect to the control port with connect-data (§1). On CONNECT event, flush
   the verify ack, set peer timeout 10/10 s. Then send **Start A** (0x0305, 2 zero bytes) and
   **Start B** (0x0307, 1 zero byte), reliable ch 0 (`ControlStream.c:1869-1935`). Don't wait
   for replies. Start the 100 ms ping loop immediately after Start B (`:1937`). Sunshine binds
   the ENet peer to the session on first contact and tears down its RTSP launch state
   (`stream.cpp:657-660`); actual video frames flow once the video UDP ping arrives.
2. **Steady state**: pings every 100 ms; input as it happens; IDR/RFI/LTR-ACK on demand;
   process rumble/HDR/LED/trigger/termination messages. Every inbound control packet must be
   GCM-verified; a failed tag on Sunshine's side kills the session (`stream.cpp:1176-1180,1223-1229`).
3. **Client-initiated stop**: stop threads, then gracefully disconnect the ENet peer with a
   **2 s linger** (`CONTROL_STREAM_LINGER_TIMEOUT_SEC`, `ControlStream.c:145,1673-1678`):
   `enet_peer_disconnect_later(peer, 0)` — waits for all outstanding reliable sends (final key-up
   events!) to be acked, then the DISCONNECT command; wait for the disconnect ack event or the
   linger timeout (`Misc.c:43-79`). The client sends no 0x0109 termination message. Sunshine
   maps `ENET_EVENT_TYPE_DISCONNECT` → `session::stop` (`stream.cpp:735-741`).
4. **Server-initiated stop**: Sunshine sends encrypted 0x0109 with `u32 BE 0x80030023` then
   flushes (`stream.cpp:1329-1358`). On receiving any termination message the client treats the
   connection as dead immediately: `enet_peer_disconnect_now(peer, 0)` (one unsequenced
   DISCONNECT, no wait — the server won't ack anything after termination) and reports the
   normalized code (`ControlStream.c:1362-1375`). GFE-quirk workaround worth copying: a raw
   ENet DISCONNECT command arriving with pending data is deferred (intercept hook drops it,
   processes remaining receives for 100 ms, then lets a retransmitted disconnect through or
   gives up after 1 s) (`ControlStream.c:886-905,1154-1193`).
5. **Loss detection**: if the client goes silent, Sunshine's 10 s ping timeout stops the
   session and abortively disconnects the peer (`stream.cpp:1273-1289`). If Sunshine dies, the
   client's ENet peer timeout (10 s) surfaces as a connection-terminated error, and the RTO/ack
   machinery is what notices (§2.3).

---

### Minimal client checklist

ENet: 2/4-byte header (peerID|session|sentTime-flag), commands CONNECT, VERIFY_CONNECT
validation, ACKNOWLEDGE (both directions), PING, SEND_RELIABLE, SEND_UNRELIABLE,
SEND_UNSEQUENCED, DISCONNECT (later + now variants); per-channel u16 reliable seq with
wraparound; RTO retransmit; 48 channels; mtu 900; no checksum, no compression, refuse
fragments. NVCTL: AES-128-GCM envelope (0x0001, LE lengths, seq→IV with 'CC'/'HC' tags, tag
before ciphertext), types 0x0305/0x0307/0x0200/0x0302/0x0301/0x0206 outbound;
0x010b/0x0109/0x010e/0x5500-0x5503 inbound; 100 ms encrypted ping; input packets with the
BE-size/LE-magic header and the per-field endianness table above; graceful disconnect with
2 s linger; treat 0x0109 as immediate teardown.

# Appendix D — video and audio framing (linked)

Sources (paths into the pinned upstream checkouts named in the preamble):
- `moonlight-common-c/src/` — the reference client (`VideoStream.c`, `RtpVideoQueue.c/.h`,
  `VideoDepacketizer.c`, `AudioStream.c`, `RtpAudioQueue.c/.h`, `Video.h`, `Limelight.h`,
  `Limelight-internal.h`, `RtspConnection.c`, `SdpGenerator.c`, `ControlStream.c`, `Connection.c`).
- `Sunshine/src/stream.cpp` — the send side (FEC encode, packetization, encryption).
- `moonlight-common-c/nanors/` — **submodule not checked out in this clone**; RS parameters below
  are taken from call sites only.

All structs below are `#pragma pack(push, 1)` — no implicit padding
(Video.h:10, Sunshine stream.cpp:90).

Scope note: everything below describes the modern path — Sunshine, or GFE ≥ 7.x
(`AppVersionQuad[0] >= 7`). Sunshine is detected by a negative 4th version component:
`IS_SUNSHINE() = AppVersionQuad[3] < 0` (Limelight-internal.h:85). Legacy Gen3–5 variances are
flagged only where the client still carries them.

---

### 1. Socket setup and pings

#### Ports

- Ports are parsed from each RTSP `SETUP` response `Transport:` header, `server_port=` entry
  (e.g. `unicast;server_port=48000-48001;source=…`, RtspConnection.c:717-746).
  Fallback defaults when parsing fails: **audio 48000** (RtspConnection.c:1194),
  **video 47998** (RtspConnection.c:1278), **control 47999** (RtspConnection.c:1322).
  RTSP itself defaults to **48010** (Connection.c:274).
- Sunshine binds them as base-port offsets: video = base+9, control = base+10, audio = base+11
  (Sunshine stream.h:19-21; default base 47989 → 47998/47999/48000).
- The client binds one local UDP socket per stream and uses it for both ping TX and RTP RX.
  Video socket asks for a receive buffer of `2048 * (packetSize + 16)` bytes
  (`RTP_RECV_PACKETS_BUFFERED` = 2048, VideoStream.c:35, 331-333). Audio socket uses default
  buffer (AudioStream.c:96).

#### Who sends first: the client. Pings are the hole punch and the address registration.

The server sends **nothing** on video/audio UDP until it receives a valid ping from the client;
Sunshine's video/audio threads block in `recv_ping()` before capture even starts
(stream.cpp:2085-2095, 2112-2122). The session's peer address/port start as `addr:0` and are
filled in from the **source address of the first matching ping** (stream.cpp:2226-2231, 2062-2064).
This is how the server learns where to send RTP — so the pings must originate from the same
socket the client will receive on.

#### Ping payload — two formats

```c
// Video.h:51-54  — fields big-endian
typedef struct _SS_PING {
    char payload[16];          // ASCII, from RTSP "X-SS-Ping-Payload"
    uint32_t sequenceNumber;   // BE32, starts at 1, +1 per ping
} SS_PING;                     // 20 bytes total
```

- **Sunshine (v2)**: RTSP `SETUP` responses for audio and video each carry an
  `X-SS-Ping-Payload` option — a 16-character ASCII string (RtspConnection.c:1203-1207 audio,
  1268-1272 video; Sunshine issues the same payload for both, stream.cpp:2277, 2314). The client
  sends the 20-byte `SS_PING` with `sequenceNumber = BE32(count)` counting 1,2,3,…
  (VideoStream.c:70-74, AudioStream.c:53-57).
- **Legacy (GFE, v1)**: exactly 4 bytes `{0x50,0x49,0x4E,0x47}` = `"PING"`
  (VideoStream.c:56,77; AudioStream.c:39,60). Used when no `X-SS-Ping-Payload` was received
  (payload[0] == 0 test at VideoStream.c:70).
- Server matching: a 4-byte datagram is matched by **source IP** (legacy clients only —
  Sunshine ignores the legacy form if the client advertised `ML_FF_SESSION_ID_V1`);
  a datagram ≥ 20 bytes is matched by the **16-byte payload** contents, ignoring the address
  entirely (stream.cpp:1432-1448, 2021-2026, 2050-2055). This means new-style clients work behind
  NAT/multi-homed paths.
- Pings are always **plaintext**, even when video/audio encryption is negotiated.

#### Interval, start, stop

- Interval: **500 ms**, both streams (VideoStream.c:80, AudioStream.c:63). Never stops — the ping
  thread runs for the life of the stream (it also serves as NAT keepalive). Send errors are
  deliberately ignored; ICMP unreachable is handled on the receive side (VideoStream.c:64-67).
- **Audio ping must start before the RTSP handshake completes**: GFE 3.22 will not answer RTSP
  `PLAY` until it has received an audio ping, so the client starts the audio ping thread as soon
  as the audio `SETUP` response is parsed (`notifyAudioPortNegotiationComplete()`,
  AudioStream.c:90-110, called from RtspConnection.c:1212).
- Video ping starts at `startVideoStream()`, before any frame is read (VideoStream.c:382-384).
- Gen3-only relic: a TCP connect to port **47996** ("first frame port") kicks off video flow
  (VideoStream.c:6, 361-380, 405-412). Not used on Gen5+/Sunshine.
- Client-side timeouts: terminate if no video packet arrives within **10 s** of stream start, or
  no *complete* frame within 10 s of the first packet (`FIRST_FRAME_TIMEOUT_SEC`,
  VideoStream.c:3-4, 146-176). Server side: Sunshine drops the session if pings/control activity
  stop for `ping_timeout` (stream.cpp:1274-1278).

---

### 2. Video RTP layout

#### Packet = [ENC_VIDEO_HEADER?] + RTP(12) + ext(4) + NV_VIDEO_PACKET(16) + payload

```c
// Video.h:42-48
typedef struct _RTP_PACKET {
    uint8_t  header;          // 0x90 for video = 0x80 (RTP v2) | 0x10 (FLAG_EXTENSION)
    uint8_t  packetType;      // (PT byte; Sunshine leaves it 0 for video — client never checks it)
    uint16_t sequenceNumber;  // BE16
    uint32_t timestamp;       // BE32, 90 kHz clock
    uint32_t ssrc;            // BE32; Sunshine sends 0 — client ignores
} RTP_PACKET;                 // 12 bytes
```

- `FLAG_EXTENSION` (0x10) in `header` means **4 extra bytes follow the 12-byte header** before the
  NV video header (RtpVideoQueue.c:553-556; the "extension" is not a real RFC 8285 extension —
  Sunshine emits it as `char reserved[4]`, stream.cpp:145). All supported servers set it; client
  asserts it (RtpVideoQueue.c:551) but tolerates its absence by using offset 12.
  `FIXED_RTP_HEADER_SIZE` 12, `MAX_RTP_HEADER_SIZE` 16 (Video.h:39-40).
- Meaningful RTP fields for video: **sequenceNumber** (FEC/reassembly position) and
  **timestamp** (PTS). `header` matters only for the extension bit. `packetType` and `ssrc` are
  ignored by the client. RTP padding is never used.
- Sunshine fill: `header = 0x80|0x10`, `seq = BE16(lowseq + x)` (continuous counter across frames,
  starting at 0 per session), `timestamp = BE32(90kHz ticks since stream epoch of the frame's
  capture time)` — same timestamp for every packet of a frame (stream.cpp:1686-1688, 1667-1675).

```c
// Video.h:27-35  — ALL FIELDS LITTLE-ENDIAN (client does LE32() on the u32s, RtpVideoQueue.c:565-567)
typedef struct _NV_VIDEO_PACKET {
    uint32_t streamPacketIndex; // LE32; only bits 31..8 meaningful: a 24-bit counter << 8.
                                //   Client: (spi >> 8) & 0xFFFFFF (VideoDepacketizer.c:757-759).
                                //   Sunshine: ((lowseq + x) << 8) — same counter as RTP seq
                                //   (stream.cpp:1631). Continuous across frames; used for corruption
                                //   detection, wraps at 2^24.
    uint32_t frameIndex;        // LE32; starts at 1, +1 per frame. Same for all packets of a frame.
    uint8_t  flags;             // FLAG_CONTAINS_PIC_DATA 0x1 | FLAG_EOF 0x2 | FLAG_SOF 0x4 (Video.h:21-23)
                                //   NB: SOF/EOF are per FEC BLOCK, not per frame (Sunshine sets
                                //   SOF on packet 0 and EOF on the last packet of EACH block,
                                //   stream.cpp:1637-1643). "First packet of frame" additionally
                                //   requires block index 0; "last" requires block == last block
                                //   (VideoDepacketizer.c:735-741, 770-771).
    uint8_t  extraFlags;        // bit 0x1 = NV_VIDEO_PACKET_EXTRA_FLAG_LTR_FRAME (Video.h:25):
                                //   frame is a long-term reference frame; client ACKs it (see §3).
    uint8_t  multiFecFlags;     // always 0x10 (Sunshine stream.cpp:1634; client forces 0x10 for
                                //   pre-multi-FEC servers, RtpVideoQueue.c:572-575). Not otherwise used.
    uint8_t  multiFecBlocks;    // bits [5:4] = current FEC block index (0-3)
                                //   bits [7:6] = last FEC block index (blockCount-1)
                                //   (client: RtpVideoQueue.c:585, 710; Sunshine: stream.cpp:1635,1690)
    uint32_t fecInfo;           // LE32 bitfield:
                                //   bits [21:12] (0x003FF000) = shard index within block (0-1023)
                                //   bits [11:4]  (0x00000FF0) = fecPercentage (0-255)
                                //   bits [31:22] (0xFFC00000) = data-shard count of this block
                                //   bits [3:0] unused
                                //   (client: RtpVideoQueue.c:584, 704-705; Sunshine:
                                //    x<<12 | data_shards<<22 | percentage<<4, stream.cpp:1681-1684)
} NV_VIDEO_PACKET;              // 16 bytes
```

- **Shard/packet size invariant**: `packetSize` (negotiated by the client via SDP
  `x-nv-video[0].packetSize`, must be a multiple of 16 — Connection.c:293-296, SdpGenerator.c:329-330)
  counts `NV_VIDEO_PACKET + video payload`. On the wire each video packet is
  `packetSize + 16` bytes (adding RTP12+ext4). Every non-EOF packet's post-NV payload is exactly
  `packetSize - 16` bytes (asserted at RtpVideoQueue.c:733); the EOF packet may be shorter.
- **fecPercentage semantics**: parity count for the block =
  `(dataShards * fecPercentage + 99) / 100` — ceiling division, identical on both sides
  (RtpVideoQueue.c:706, Sunshine stream.cpp:813). Sequence-number layout of a block:
  `[base .. base+data-1]` data shards, `[base+data .. base+data+parity-1]` parity shards. A packet
  is parity iff `seq >= bufferFirstParitySequenceNumber` (RtpVideoQueue.c:707, 734). Block base is
  recovered from any packet: `base = seq - fecInfo.shardIndex` (RtpVideoQueue.c:696).

#### Optional video encryption (Sunshine `SS_ENC_VIDEO`)

Negotiated via RTSP: DESCRIBE response advertises `x-ss-general.encryptionSupported` /
`…encryptionRequested` bitmask (0x01 control-v2, 0x02 video, 0x04 audio — Limelight-internal.h:48-50,
RtspConnection.c:1150-1155); the client answers with `x-ss-general.encryptionEnabled` in its
ANNOUNCE SDP (SdpGenerator.c:277-304). When video encryption is on, the client shrinks
`packetSize` by 32 to keep wire MTU constant (`packetSize -= sizeof(ENC_VIDEO_HEADER)`,
SdpGenerator.c:323-328).

```c
// Video.h:15-19 — prepended to every encrypted video datagram (32 bytes, multiple of 16 on purpose)
typedef struct _ENC_VIDEO_HEADER {
    uint8_t  iv[12];       // AES-GCM IV
    uint32_t frameNumber;  // LE32, plaintext copy of frameIndex (pre-decryption drop filter only)
    uint8_t  tag[16];      // GCM auth tag
} ENC_VIDEO_HEADER;
```

- Cipher: **AES-128-GCM**, key = `StreamConfig.remoteInputAesKey` (the 16-byte RI key from the
  HTTPS /launch handshake), ciphertext = the entire RTP packet (RTP hdr + ext + NV hdr + payload,
  `blocksize = packetSize+16` bytes) (VideoStream.c:213-221, Sunshine stream.cpp:1711).
- IV construction (Sunshine, stream.cpp:1694-1711): 12 bytes = LE64 monotonic per-packet counter
  in bytes 0-7, bytes 8-10 zero, byte 11 = ASCII `'V'`. The client treats the IV as opaque.
- Client fast-path: before decrypting, drop the datagram if plaintext `frameNumber != 0` and
  `< currentFrameNumber` (VideoStream.c:209-211) — untrusted, but only used to skip work.

---

### 3. Frame reassembly, FEC, and loss recovery (RtpVideoQueue + VideoDepacketizer)

#### Queue state machine (RtpVideoQueue.c `RtpvAddPacket`, 544-805)

Reject early: `seq < nextContiguousSequenceNumber` (behind window, :545), too short for NV header
(:558), `frameIndex < currentFrameNumber` (16-bit-style wrap compare on the 32-bit value via
`isBefore16`, :578), FEC block index behind current (:587).

A (frame, blockIndex) change **re-arms the buffer** (:594-714):
- If the previous frame's pending block was incomplete → it is unrecoverable: report final FEC
  status (§ SS_FRAME_FEC_STATUS), purge, `notifyFrameLost()`, advance (:596-663).
- Missing an entire intermediate frame → `notifyFrameLost(frameIndex-1)` (:676-687).
- New-block init derives everything from this one packet:
  `bufferLowestSequenceNumber = seq - fecIndex`, `bufferDataPackets = fecInfo[31:22]`,
  `fecPercentage = fecInfo[11:4]`, `parity = (data*pct+99)/100`,
  `bufferHighestSequenceNumber = lowest + data + parity - 1` (:695-710).
- Packets above the block's highest seq are rejected (:716-719).

Duplicates are dropped via list scan (fast path skips the scan while the stream is in-order,
:127-151). Missing-packet count is maintained incrementally from seq-number gaps (:739-757).

#### Reordering

There is **no reorder timer for video**. Order within a block doesn't matter (shards are indexed
by `seq - base`); a block is complete as soon as *any* `dataShards` of its `data+parity` shards
arrive. Loss is declared only when (a) prediction says the block can't complete (below), or
(b) a packet from the next block/frame arrives while the current one is incomplete.

#### FEC recovery (reconstructFrame, RtpVideoQueue.c:194-464)

- RS code: `reed_solomon_new(dataShards, parityShards)` per block; shard length =
  `packetSize + 16` (the **full RTP packet including headers** is the RS symbol block,
  :269, 281, 332). Received shards shorter than that (the EOF packet) are zero-padded before
  decode (:313-316). `reed_solomon_decode(rs, packets, marks, totalShards, shardLen)` with
  `marks[i]=1` for missing.
- If all data shards arrived, no decode is done (:248-251). FEC recovery requires Gen5+ (:254-258).
- Recovered data shards get their RTP/NV headers patched: seq = `base+i`, header/timestamp/ssrc
  copied from an existing packet of the block, `frameIndex` and `multiFecBlocks` re-stamped
  (:349-370). Sanity checks on recovered packets (SOF on shard 0, EOF on last, CONTAINS_PIC_DATA
  in the middle, no unknown flags) or the packet is discarded (:428-439). Recovered packets carry
  trailing zero padding — deliberately not stripped for H.264/HEVC (:441-447).
- Data shards (never parity) of a completed block are handed to the depacketizer in seq order
  (`stageCompleteFecBlock`/`submitCompletedFrame`, :466-538). Multi-block frames only reach the
  depacketizer once **all** blocks completed (:784-800).

#### Sender-side FEC block construction (Sunshine stream.cpp:1543-1601, fec::encode 806-882)

- Frame payload = 8-byte frame header + bitstream, chopped into `packetSize-16` slices with 32
  bytes of header space inserted before each (concat_and_insert, :1546-1550).
- `fecPercentage` = host config `fec_percentage` (default 20, valid 1-255, config.cpp:1758),
  bumped up per-block if parity < client's `minRequiredFecPackets` (client requests **2** via SDP
  `x-nv-vqos[0].fec.minRequiredFecPackets`, SdpGenerator.c:211; bump math stream.cpp:816-820 —
  can reach 200% for a 1-packet frame).
- Max data shards per block: `(255*100)/(100+fecPercentage)` (DATA_SHARDS_MAX=255 total shards
  per RS block, stream.cpp:1555-1562). Frame split into
  `ceil(payload / (maxShards*blocksize))` blocks, each aligned to blocksize (:1566, 1582-1601).
  **Max 4 blocks** (2-bit field). If a frame would need >4 → FEC disabled for the frame
  (`fecPercentage=0`, parity=0, still ≤4 blocks) (:1568-1574). Shard index is 10 bits → >1024
  packets per block is unrecoverable (error log :1586-1590).
- Data shards point into the payload; only the (zero-padded) final data shard and parity shards
  are separately allocated (:826-857). Parity = `reed_solomon_encode(rs, shards_p, nr, blocksize)`
  (:866-868).

#### RS implementation parameters (nanors)

Call-site contract (both sides use the same `rs.h` API): `reed_solomon_init()` once,
`reed_solomon_new(k_data, m_parity)`, `reed_solomon_encode(rs, uint8_t** shards, k+m, shardLen)`,
`reed_solomon_decode(rs, shards, uint8_t* marks, k+m, shardLen)`; GF(2^8), max 255 total shards
(`DATA_SHARDS_MAX`, stream.cpp:1555-1562). The nanors submodule is not present in this clone —
for a Rust clean-room, the binding requirement is: **video parity must match nanors' default
generator matrix construction; audio parity uses an explicitly overridden matrix** (see §5). Pin
nanors as the test oracle to capture golden shards.

#### When the client asks for recovery — RFI vs IDR

Two mechanisms; both go over the ENet control channel (not the RTP socket):

- **Reference Frame Invalidation (preferred)** — packet type `0x0301`
  (ControlStream.c:206, Gen5+ including Sunshine; Sunshine handler stream.cpp:1144-1155):
  ```c
  // Video.h:80-86 — fields little-endian, 24 bytes
  typedef struct _SS_RFI_REQUEST {
      uint32_t firstFrameIndex;  // LE32 — start of RFI window (frame after last complete frame)
      uint32_t reserved1;        // 0
      uint32_t lastFrameIndex;   // LE32 — last lost frame
      uint32_t reserved2[3];     // 0
  } SS_RFI_REQUEST;
  ```
  Sent reliable on ENet channel 1 (`CTRL_CHANNEL_URGENT`) (ControlStream.c:1538-1560). Note
  Sunshine reads the two fields as int64s at offsets 0 and 8 (stream.cpp:1145-1147) — same layout.
  Used when the decoder supports RFI **and** an IDR frame has already been decoded; queued RFI
  ranges are aggregated (ControlStream.c:1597-1620). The host answers with a P-frame of
  frame-header type 5 that doesn't reference the invalidated range.
- **IDR request** — packet type `0x0302`, payload 2 zero bytes, reliable, channel 1
  (ControlStream.c:203-204, 228, 1521-1533). Used when RFI is unsupported/disabled, when no IDR
  has been decoded yet, when the decode-unit queue overflows, or after
  `CONSECUTIVE_DROP_LIMIT` = **120** consecutively dropped frames (VideoDepacketizer.c:30,
  106-128). On servers without an IDR request type the client fakes it with an RFI of the last
  32 frames (ControlStream.c:1490-1520).
- **Trigger points**: `notifyFrameLost()` from the RTP queue on an unrecoverable block
  (speculative — predicted as soon as `missingPackets > remaining recoverable margin` if no
  recent out-of-order data was seen, RtpVideoQueue.c:210-241; speculation disabled for 5 min
  after any OOS packet, `SPECULATIVE_RFI_COOLDOWN_PERIOD_US` = 300 s, :14-15, 161-178) —
  or non-speculative when a newer frame/block preempts an incomplete one (:619, 654). The
  depacketizer also triggers on stream-packet-index discontinuities (corrupt frame,
  VideoDepacketizer.c:785-798) and on missing frames at SOF (:805-826).
- **Frame-FEC status report (Sunshine extension)** — type `0x5502`, sent ENet *unsequenced*
  channel 0 whenever a frame needed recovery or was dropped, gated on `IS_SUNSHINE()` and
  advertised via `x-ml-general.featureFlags` bit `ML_FF_FEC_STATUS` 0x01 (SdpGenerator.c:272,
  ControlStream.c:455-468, 1406-1421):
  ```c
  // Video.h:57-70 — fields BIG-endian, 21 bytes
  typedef struct _SS_FRAME_FEC_STATUS {
      uint32_t frameIndex;                        uint16_t highestReceivedSequenceNumber;
      uint16_t nextContiguousSequenceNumber;      uint16_t missingPacketsBeforeHighestReceived;
      uint16_t totalDataPackets;                  uint16_t totalParityPackets;
      uint16_t receivedDataPackets;               uint16_t receivedParityPackets;
      uint8_t  fecPercentage;                     uint8_t  multiFecBlockIndex;
      uint8_t  multiFecBlockCount;
  } SS_FRAME_FEC_STATUS;   // filled in reportFinalFrameFecStatus(), RtpVideoQueue.c:93-109
  ```
- **LTR ACK (Sunshine extension)** — when a received frame has `extraFlags & 0x1` (LTR), client
  sends type `0x0350`, payload `{uint32 frameIndex LE; uint32 reserved}` (8 bytes), reliable
  channel 1 (Video.h:73-77, ControlStream.c:431-452, 1562-1580).

---

### 4. Bitstream handoff (VideoDepacketizer.c)

#### Frame header (first packet of every frame, after the NV header)

Sunshine/GFE ≥ 7.1.450 with first byte `0x01` → **8 bytes** (`0x81` → 44 bytes on GFE;
version table for older GFE: 8/12/24/41/44 bytes, VideoDepacketizer.c:914-965). Sunshine always
sends the 8-byte "short" header (stream.cpp:95-129):

```
offset 0: 0x01                 headerType (short header)
offset 1: LE16                 frame_processing_latency, 0.1 ms units (Sunshine ext; client reads
                               it only when IS_SUNSHINE(), VideoDepacketizer.c:899-903)
offset 3: u8                   frameType: 1=P, 2=IDR, 4=intra-refresh, 5=P after RFI
                               (104 = old Sunshine hardcoded header, ignored)
offset 4: LE16                 lastPayloadLen — length of the final packet's payload incl. this
                               header (Sunshine ext for codecs intolerant of zero padding, i.e. AV1)
offset 6: u8[2]                reserved/zero
```

The client strips the frame header from the first packet (VideoDepacketizer.c:967-972); later
packets of the frame have no header (:1000-1003). Frame-header `frameType` drives the RFI wait
state; for non-H.264/HEVC codecs type 2 alone marks IDR (:857-895).

#### What the decoder receives

- **H.264/HEVC**: Annex B **with start codes kept** (4-byte `00 00 00 01` at frame start; the
  client never rewrites start codes). Processing on the first packet: strip frame header, then
  strip one optional AUD NAL and any SEI NALs (:975-998). If the frame starts with SPS (H.264) /
  VPS (HEVC) it's an IDR frame → the slow path splits the prefix into separate buffers typed
  `BUFFER_TYPE_SPS/PPS/VPS` (padding between them skipped), with the picture data (IDR slice
  onward) as `BUFFER_TYPE_PICDATA` (:1005-1008, 654-713). A PPS prepended to a P-frame (Intel
  MFX quirk) is stripped (:1013-1015). Everything else is passed through byte-exact, including
  FEC trailing zero padding (legal per Annex B, RtpVideoQueue.c:441-444).
- **Codec parameter sets are in-band only**: every IDR frame from the host starts
  AUD? SEI* SPS PPS (H.264) / VPS SPS PPS (HEVC) then slices — validated by
  `validateDecodeUnitForPlayback` (:199-224). There is no out-of-band extradata.
- **AV1 (and any non-H26x codec)**: no bitstream parsing at all — buffers are `BUFFER_TYPE_PICDATA`
  (:558-560), IDR-ness comes from frame header type 2, and the **final packet is truncated to
  `lastPayloadLen - frameHeaderSize`** because AV1 can't tolerate trailing zeros
  (:1030-1064; invalid length ⇒ frame dropped + loss reported).
- Decode unit metadata: `frameNumber = frameIndex`; `frameType` IDR/P;
  `presentationTimeUs = rtpTimestamp * 1000 / 90` (90 kHz → µs, RtpVideoQueue.c:17-18, 158);
  if the server sends timestamp 0 (old Sunshine), PTS is synthesized from arrival time relative
  to the first frame (VideoDepacketizer.c:834-846). `receiveTimeUs` = arrival of the block's
  first packet (all packets of a block share it, RtpVideoQueue.c:495-503).

---

### 5. Audio

#### RTP layout

Audio data packet = 12-byte RTP header + Opus payload (encrypted or not). No extension.

- `header = 0x80`, `packetType = 97` (data) (Sunshine stream.cpp:1811-1813;
  `RTP_PAYLOAD_TYPE_AUDIO` RtpAudioQueue.c:20).
- `sequenceNumber` BE16, +1 per data packet (FEC packets do **not** consume data seq space —
  see below). `timestamp` BE32 increments by `packetDuration` (i.e. **units are milliseconds**,
  +5 per packet at 5 ms) (stream.cpp:1842-1846). `ssrc = 0`.
- Max datagram the client accepts: **1400 bytes** (`MAX_PACKET_SIZE`, AudioStream.c:26).

Audio FEC packet = RTP header (`packetType = 127`) + `AUDIO_FEC_HEADER` + parity payload:

```c
// RtpAudioQueue.h:18-24 — fields big-endian on the wire
typedef struct _AUDIO_FEC_HEADER {
    uint8_t  fecShardIndex;       // 0 or 1
    uint8_t  payloadType;         // 97 (payload type of the protected data packets)
    uint16_t baseSequenceNumber;  // BE16, seq of first data packet in the block (multiple of 4)
    uint32_t baseTimestamp;       // BE32, timestamp of first data packet
    uint32_t ssrc;                // 0
} AUDIO_FEC_HEADER;               // 8 bytes
```

Sunshine's FEC RTP header: `header=0x80, packetType=127, timestamp=0, ssrc=0`,
`sequenceNumber = BE16(lastDataSeqOfBlock + shardIndex + 1)` — these seq numbers **collide with
the next block's data packets**; the client ignores the RTP seq of FEC packets and uses only
`fecHeader.baseSequenceNumber + fecShardIndex` (stream.cpp:1862-1889, 2301-2307;
RtpAudioQueue.c:242-283).

#### Opus configuration — negotiated in RTSP, nothing in-band

- Sample rate always **48000 Hz** (RtspConnection.c:754). `samplesPerFrame = 48 * packetDuration`
  (AudioStream.c:440).
- Frame duration: chosen by the **client** and sent in ANNOUNCE SDP `x-nv-aqos.packetDuration` —
  5 ms default, 10 ms for slow decoders / <5 Mbps links with
  `CAPABILITY_SUPPORTS_ARBITRARY_AUDIO_DURATION` (SdpGenerator.c:493-527). One Opus frame per RTP
  packet.
- Channel config: client requests via SDP `x-nv-audio.surround.numChannels/channelMask/enable`
  (audioConfiguration word = `(mask<<16)|(channels<<8)|0xCA`, Limelight.h:208-209,
  SdpGenerator.c:481-490). Stereo is hardcoded: 2 ch, 1 stream, 1 coupled, mapping {0,1}
  (RtspConnection.c:756-763). Surround: the RTSP **DESCRIBE** response carries
  `a=fmtp:97 surround-params=<C><S><P><mapping…>` — single ASCII digits: channelCount, streams,
  coupledStreams, then `C` mapping digits (RtspConnection.c:678-716, 766-830). High-quality
  surround config = a **second occurrence** of the same `surround-params=<C>` prefix later in the
  DESCRIBE payload (RtspConnection.c:801-817); its presence sets `HighQualitySurroundSupported`,
  and the client opts in (`x-nv-audio.surround.AudioQuality=1`) when bitrate ≥ 15 Mbps and the
  decoder is fast; HQ implies 5 ms frames and disables coupled streams client-side
  (SdpGenerator.c:493-504, Limelight-internal.h:93-99). If no `surround-params` at all, 5.1 falls
  back to a hardcoded config: 4 streams, 2 coupled, mapping {0,4,1,5,2,3} (RtspConnection.c:820-840).
- GFE channel-mapping quirk: for 5.1/7.1 normal quality, the client swaps the received mapping
  (mapping[3]↔last, shifting the rest) to get its expected channel order (RtspConnection.c:786-799).

#### Audio encryption — AES-128-CBC, per-packet

Enabled when: GFE path — client sets `NVFF_AUDIO_ENCRYPTION` (0x20) in
`x-nv-general.featureFlags` (SdpGenerator.c:177-198); Sunshine path — `SS_ENC_AUDIO` (0x04)
negotiated in `x-ss-general.encryptionEnabled` (SdpGenerator.c:293-304). Client-side flag:
`AudioEncryptionEnabled` (AudioStream.c:178).

- Cipher: **AES-128-CBC with PKCS#7 padding**, key = `remoteInputAesKey` (16 bytes; same RI key
  as everything else) (AudioStream.c:192-197; Sunshine uses `crypto::cipher::cbc_t{gcm_key, true}`,
  stream.cpp:2309-2312, encrypt at 336-344). Ciphertext length = plaintext rounded up to 16
  (a full 16-byte pad block is added if already aligned — standard PKCS#7;
  `ROUND_TO_PKCS7_PADDED_LEN(x) = ((x+15)/16)*16`, PlatformCrypto.h:22).
- **IV (16 bytes)**: bytes 0-3 = `BE32(avRiKeyId + rtpSequenceNumber)`, bytes 4-15 = zero.
  `avRiKeyId` = the first 4 bytes of `remoteInputAesIv` (the RI IV from the launch handshake)
  interpreted as a big-endian u32 (AudioStream.c:80-82, 186-190; Sunshine stream.cpp:1830, 2315).
  Addition is plain u32 arithmetic, then re-serialized big-endian.
- Only the Opus payload is encrypted; the RTP header is plaintext. **FEC parity is computed over
  the ciphertext** (encryption happens before `reed_solomon_encode`, stream.cpp:1834, 1871), so
  recovered shards must be decrypted after RS recovery using the reconstructed seq number.

#### Audio FEC — fixed RS(4 data, 2 parity)

- Constants: `RTPA_DATA_SHARDS` 4, `RTPA_FEC_SHARDS` 2 (RtpAudioQueue.h:11-13). Blocks always
  start at `seq % 4 == 0`; the client synthesizes the block identity of a data packet as
  `base = (seq/4)*4`, `baseTs = ts - (seq-base)*packetDuration` (RtpAudioQueue.c:236-240).
- Shard length = this block's Opus payload length (all 4 data packets of a block must be
  equal-sized; mismatch ⇒ client disables audio FEC entirely as "incompatible server",
  RtpAudioQueue.c:314-323). Sunshine encodes parity over `bytes` = current packet's payload size
  (stream.cpp:1871).
- **The RS generator matrix is NOT nanors' default**: both sides overwrite the 2×4 parity
  submatrix `rs->p` with `{0x77,0x40,0x38,0x0e,0xc7,0xa7,0x0d,0x6c}` (row 0 = 77 40 38 0e,
  row 1 = c7 a7 0d 6c) — the OpenFEC/Nvidia-compatible matrix (RtpAudioQueue.c:54-61,
  stream.cpp:1803-1809). A Rust client must decode against exactly this matrix.
- Recovery: possible once `dataReceived + fecReceived >= 4`
  (RtpAudioQueue.c:401-411). Recovered data packets get a synthesized RTP header:
  `header=0x80, packetType=fecHeader.payloadType, seq=base+i, ts=baseTs+i*packetDuration,
  ssrc=fecHeader.ssrc` (:456-466).
- In-order data packets short-circuit the queue and go straight to the decoder
  (`RTPQ_RET_HANDLE_NOW`, :604-622); the FEC block machinery only engages on loss/reorder.

#### Loss handling / PLC

- Give-up rule (`handleMissingPackets`, RtpAudioQueue.c:517-564): if the awaited packet precedes
  the oldest queued block, resync to that block. Otherwise wait until a *second* FEC block is
  queued behind the incomplete one; then give up immediately if no out-of-order data has ever
  been seen from this host ("fast recovery mode"), else wait until the block has been queued
  longer than `4*packetDuration + 10 ms` (`RTPQ_OOS_WAIT_TIME_MS`, RtpAudioQueue.h:7-9).
- On give-up the block is drained with `allowDiscontinuity`: present packets are returned in
  order; each missing one is returned as a **zero-length placeholder**, for which the client
  calls the Opus decoder with a NULL buffer to trigger packet-loss concealment
  (RtpAudioQueue.c:662-703, AudioStream.c:163-169).
- OOS bookkeeping mirrors video: an out-of-sequence data packet leaves fast-recovery mode;
  32767 in-order packets re-enter it (RtpAudioQueue.c:216-232).
- There are **no audio config packets** on the RTP stream and no IDR-equivalent — recovery is
  purely FEC + PLC. (Client debug builds assert the Opus TOC byte stays constant; Sunshine may
  legitimately violate this for surround, AudioStream.c:203-216.)
- Startup: the client drops the first `500 / packetDuration` **data** packets (500 ms) to shed
  the backlog GFE accumulates before the client is ready (AudioStream.c:248, 297-318).

---

### 6. Queue/timing behavior worth copying

| Thing | Value | Cite |
|---|---|---|
| Ping interval (video & audio) | 500 ms, forever | VideoStream.c:80, AudioStream.c:63 |
| UDP recv poll timeout | 100 ms (`UDP_RECV_POLL_TIMEOUT_MS`) | Limelight-internal.h:91 |
| First-video-packet / first-frame timeout | 10 s | VideoStream.c:3-4,146-176 |
| Video socket recv buffer | 2048 × (packetSize+16) | VideoStream.c:29-35,331-333 |
| Video reorder window | none by time; per-FEC-block by seq range; frame abandoned when next frame/block arrives or loss is provably unrecoverable | RtpVideoQueue.c:210-241,594-663 |
| Speculative-RFI cooldown after OOS | 300 s | RtpVideoQueue.c:14-15 |
| Consecutive frame-drop limit → force IDR | 120 frames | VideoDepacketizer.c:30,119-128 |
| Video decode-unit queue depth | 15 frames (overflow ⇒ flush + IDR) | VideoDepacketizer.c:63,513-533 |
| Audio OOS wait after block due | blockDuration(4×pkt) + 10 ms | RtpAudioQueue.c:547, RtpAudioQueue.h:7-9 |
| Audio decode queue depth | 30 packets (overflow ⇒ flush all, keep newest) | AudioStream.c:69,142-160 |
| Audio startup drop | 500 ms of data packets | AudioStream.c:248 |
| Audio FEC block cache | 4 free blocks | RtpAudioQueue.h:16 |
| Control periodic ping | type 0x0200, payload 8 bytes LE {u16 4, u32 0, pad}, every 100 ms, reliable | ControlStream.c:297-298,1391-1443 |
| Seq comparisons | serial-number arithmetic: `isBefore16/24/32(x,y) = ((x-y) & MAX) > MAX/2` | Limelight-internal.h:72-78 |

Misc facts a client must honor:
- `frameIndex` starts at 1 (`currentFrameNumber = 1`, RtpvInitializeQueue, RtpVideoQueue.c:24)
  and the depacketizer expects frame 1 first (VideoDepacketizer.c:65).
- Multi-FEC capability is a client property keyed off server version ≥ 7.1.431; for older
  servers the client fakes `multiFecFlags=0x10, multiFecBlocks=0` (RtpVideoQueue.c:25, 572-575).
- The client should keep receiving/discarding on the video socket even while dropping — the
  server's pacing assumes ~80% of 1 Gbps bursts in ≤64 KB batches (Sunshine stream.cpp:1604-1615).
- QoS/DSCP: client asks for tagging via SDP `x-nv-vqos[0].qosTrafficType 5` / `x-nv-aqos… 4` on
  LAN (SdpGenerator.c:400-407); Sunshine tags sockets accordingly (stream.cpp:2090-2092, 2117-2119).
