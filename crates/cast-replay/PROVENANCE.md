# Provenance: how everything in this crate was derived

Four things in `cast-replay` came out of reverse engineering someone else's
product, and none of them can be re-derived from this repository alone:

1. the **CKS static table** (`fixtures/`) — AirReceiver's identity;
2. the **CKS remote pull** (`src/api.rs`) — its backend request and response cipher;
3. the **AirServer static database** (`fixtures/airserver/`) — AirServer's identity;
4. the **AirServer remote pull** (`src/airserver_api.rs`) — its endpoint.

This file records where each came from in enough detail to do it again. It exists
because the tooling that produced them lives in `re-shell/artifacts/`, which is
**gitignored in that repository** — so those scripts are not under version control
and cannot be assumed to survive. If they are gone, this is what is left, and it
should be enough to rebuild them.

Ground rule 9 applies throughout: what lands here is reimplementations, not bytes.
Nothing in this tree links, ships or shells out to either product — and as of the
carve work, nothing in it *carries* either product's material either. Both offline
identities and all four sets of constants are recovered at build time by
`nix/airserver-carve.nix` and `nix/airreceiver-carve.nix`; a build without them has
no offline Cast identity and says so. This file remains the record of how the
recovery works, and is the only place the recovered values are written down.

## Validation

The six constants above are not written down here. They belong to SoftMedia and App
Dynamic, this file is the one place they would otherwise survive in a public
repository, and the carves do not need them: each proves its own answer.

* the AirServer constants are confirmed by a **Poly1305 tag** — a wrong pair cannot
  authenticate a secretbox, so `airserver-carve.py` either emits the right values or
  fails;
* the CKS identity is confirmed by verifying **all 900 windows'** receiver-auth
  signatures against the shipped device certificate before anything is written;
* the AirPort key is confirmed by its **modulus**;
* the CKS field cipher is confirmed only by the **digests below**, which is the weakest
  of the four and is called out as such in `carve_field_cipher`. A digest proves a new
  build carries the same bytes that were first read out of Ghidra; it does not prove
  those bytes are the field cipher. The hand recovery remains the load-bearing step.

What is left here is for a human cross-check: enough to tell whether a carve on a new
build produced the same thing as the one this file documents, without the file being
grep-able for the values themselves.

| | what | sha256 |
|---|---|---|
| S1 | the `sig` secret, as the 32 ASCII characters it is used as | `8e852a996b75117f76c41c59c69673410ee8f23a18de7fc3b1a8c695de843363` |
| S2 | the `x-api-key` value, likewise | `eae79fd0e887ea1d59844bae63a7beef73d1e5758e912dc60b6ff9faa6ebf961` |
| S3 | the response field cipher key, 16 raw bytes | `2f83f3a7b71cf17a5511e500ddad0c8297afb2ae6deacccb7ac014909b14c987` |
| S4 | its counter block, 16 raw bytes | `d827af29149d8911eafbb151649c81992712987febd228b864209a769fb27946` |
| S5 | the BLAKE2b personalisation, 16 ASCII bytes | `3121c9e6a028a5bed7d46ed1c6db2c140e2d2c17c8bbf43f38c113a670bb08cb` |
| S6 | the BLAKE2b key, 64 ASCII bytes | `00bfb779275c89f863a4d2c728c6fbad7e886b342e7ed64123e85ec1b1d98b31` |

All six are carve outputs verbatim, so they check with two commands:

```sh
sha256sum "$(nix build --no-link --print-out-paths .#airreceiver-carve)"/cks_{sig_secret,api_key}.txt \
          "$(nix build --no-link --print-out-paths .#airreceiver-carve)"/cks_field_{key,iv}.bin
sha256sum "$(nix build --no-link --print-out-paths .#airserver-carve)"/kek_{person,pass}.bin
```

S3 and S4 joined the carve later than the rest. They were left in `src/api.rs` as byte
literals when the others were removed, on the reasoning that a fixed key *and* fixed IV
reused for every field is an encoding rather than encryption and authorises nothing —
which is true, and is beside the point: it left this repository redistributing two of
SoftMedia's constants and grep-able for them. They are recovered the same way as the
rest now, and `CksCredentials` carries all four together.

One derived value is kept in the clear deliberately, in §2's live-request example:
`sig` for `ts=1700000000` is `6f12c3bf54f1ddd603e7d0a4478c378b`. It is the output of a
one-way function over a 128-bit secret, so it discloses nothing, and it is the single
most useful thing in this file — it validates the *whole* derivation (secret first,
decimal timestamp, lowercase hex) end to end. `api.rs` asserts exactly that vector.

## Source artifacts

Exact inputs, with SHA-256 so a future re-derivation can confirm it is working from
the same bytes rather than a later build that moved everything.

| Artifact | Size | SHA-256 |
|---|---|---|
| `com.softmedia.receiver.lite.apk` (AirReceiver Lite **5.1.7**) | 12 810 825 | `79fcae57ea65b56d212e6f020e4230eb69fe399c1030476f462ecd6af04851c8` |
| └ `lib/arm64-v8a/libAirReceiver.so` | 11 016 352 | `71701f4fc335e14a504674454580baa8e6e4528ad0de28f1cfbc5ca34c398974` |
| `AirServer-5.7.2-x64.msi` | 20 738 048 | `173685e9c546993d9956030587ee296b61947e8944878538abd5a598b91a9bfc` |
| └ `AirServer.exe` (**5.7.2**) | 23 779 840 | `134a95c53b105e9d48f34289ea5694fcb836ecb751f31e62123c45ef50e6a7df` |
| `AirServer-2025.7.23-x64.msix` | 105 967 771 | `2fd5fbd509d5b557ec442c3f628c443ba55a8b587a2ccf82d11858a78e52ef45` |
| └ `AirServer.exe` (**2025.7.23**) | 17 816 064 | `b3c5379079befab2c0143f95eba3b34bf667996da26605deaddcee4696e11f83` |
| `builtin_castdb_win5.7.2.sqlite` (extracted) | 5 488 640 | `50cd8387f0bb47d1e4808f827eba3efe3cd569d317383532ca2d0d932502ab40` |

The APK came from an `AirReceiverLite_latest.xapk` bundle (an XAPK is a zip of
APKs; the base APK is the one above). AirServer installers came from
`download.airserver.com`. **Version matters**: the addresses below are specific to
these builds, and the 4.9.7 armeabi-v7a AirReceiver build uses a *different* `dbio`
password or digest and does not unwrap under 5.1.7's key.

## 1. The CKS static table

Depends on: `fixtures/{device_cert.pem,ica.pem,peer_template.der,peer_key.der,signatures_sha1.bin,signatures_sha256.bin}`.

All addresses are **Ghidra** addresses at image base `0x100000`; subtract
`0x100000` for an ELF virtual address.

The table does not sit in the binary in the clear, which is why a plaintext sweep
and an RSA-padding sweep both missed it: it ships inside a `dbio` container that is
AES-128-CTR encrypted end to end.

```
dbio blob at ELF 0x195184, payload used through 0x7c2a1
container: 4-byte 'dbio' magic; +0x10 holds a 16-byte file key,
           itself AES-128-ECB encrypted under a KEK
KEK      = H("com.softmedia.receiver")
```

`H` is a 128-bit digest built from three hash primitives whose function pointers
are assembled at runtime in `.data`, so it is not practical to read off a
decompiler. It was recovered by **emulating** them with Unicorn, which also
recovers the password itself — that is what `dbio_kek.py` does. Once the KEK is
known, `cks_store.py` opens the container and walks the window table.

```sh
python3 dbio_kek.py  libAirReceiver.so        # -> password b'com.softmedia.receiver'
python3 cks_store.py libAirReceiver.so --openscreen
# dbio blob at 0x195184, payload used through 0x7c2a1
# windows 900 (2023-01-01 .. 2027-12-06, 2-day)
```

Result: 900 windows, 1800 signatures, one peer key, one peer certificate template.
Unlike AirServer's, these per-window certificates differ **only** in validity, so
`src/template.rs` re-issues them from one template rather than storing 900 copies.

## 2. The CKS remote pull

Reimplemented in `src/api.rs`. Recovered from the same `libAirReceiver.so`.

`FUN_0041efb0` constructs the CKS client (vtable `0xa3be38`, 28 slots) and embeds a
**provider** at `+0xe8` (vtable `0xa3bf28`). The provider's fields are the whole
protocol:

| Provider field | Contents |
|---|---|
| `+0x08` | `vector<string>` of two pinned root certificates |
| `+0x20` | `https://cast.remotetogo.com/api/v1/cks?ts=%s&sig=%s` |
| `+0x38` | `AirReceiver/1.0.0 CrKey/1.0` (User-Agent) |
| `+0x50` | the `sig` secret — 32 ASCII hex characters ([S1](#validation)) |
| `+0x68` / `+0x80` | `x-api-key` and its value — 32 ASCII hex characters ([S2](#validation)) |
| `+0x140` | server clock offset, `now − time(NULL)` |
| `+0x148` | timestamp of the last failed fetch (the 360 s backoff) |

Provider vtable slots: 2 = fetch (`FUN_00423f60`), 3/4 = the field cipher
(`FUN_00425120`, `FUN_0042524c`), 5 = corrected clock (`FUN_00425258`),
6 = `md5hex` (`FUN_00424fc4`). The request is built in slot 2:

```
ts  = snprintf("%lld", time(NULL))
sig = lowercase_hex(MD5(<the +0x50 secret> + ts))
url = snprintf("https://cast.remotetogo.com/api/v1/cks?ts=%s&sig=%s", ts, sig)
```

Secret **first** — `FUN_00424eb4` copies provider `+0x50` then appends `ts`. So
`sig` is a bare `MD5(secret || ts)`: no nonce, no request binding, no keyed MAC,
and the backend does not check that `ts` is recent. That is why `src/api.rs` can
mint a request for any timestamp, and it is a property of their design, not a
choice of ours.

Each of the six returned values is **base64, then AES-128-CTR** under constants
immediate in `.rodata` (`FUN_00425120`):

```
key = <16 raw bytes at ELF 0x1413b6>   ([S3](#validation))
iv  = <16 raw bytes at ELF 0x125820>   ([S4](#validation))
```

Both are bare immediates with nothing nearby to anchor on, so `carve_field_cipher`
finds them by hashing every 16-byte window of `.rodata` against the digests above
rather than by address; the offsets here are for a human reading the binary. Two
properties hold in 5.1.7 and are asserted rather than assumed: each occurs exactly
once in the whole image, and `iv == key ^ 0x10` byte-wise — the latter almost
certainly just how the pair was generated, so it corroborates and does not identify.

The same cipher wraps the client's own on-disk `cks2` cache record — a fixed key
*and* fixed IV, reused for every field of every record. `fixtures/pinned_roots.pem`
is provider `+0x08`: Starfield Class 2 and Amazon Root CA 1, the CloudFront chain
for `cast.remotetogo.com`, pinned in place of the system store.

Confirmed live with two GETs on 2026-07-29 (`cks_api.py --fetch`).

## 3. The AirServer static database

Depends on: `fixtures/airserver/airserver_*.{der,bin,json}`.

The database is linked into `AirServer.exe` as the "builtin db" (the giveaway is the
string `Reverting to builtin db`) in **both** the 5.7.2 MSI and the 2025.7.23 MSIX,
and they carve to byte-identical files. An earlier revision of this file said 5.7.2
shipped it as a loose encrypted SQLite file inside the MSI; it does not — the WiX
payload cab holds `AirServer.exe` and its DLLs and nothing database-shaped.

It arrived in **5.7.0**: 5.6.1 and earlier contain no SQLite image at all. 5.7.0's is a
different, smaller file (2 936 832 bytes, `75ddb897…`, eight tables) that carries the
same 1095 windows but has no `metadata` table; 5.7.1 and 5.7.2 both carry the
5 488 640-byte nine-table file below. So `metadata` is a schema addition, not a
constant of the format, and nothing may gate on its presence.

Carving is exact rather than heuristic, because a SQLite header carries its own
length — page size at `+16` (big-endian u16, `1` meaning 65536) and page count at
`+28`:

```sh
# 5.7.2: loose file out of the MSI
msiextract AirServer-5.7.2-x64.msi          # or 7z x
# 2025.7.23: carve it out of the executable
airserver_castdb.py --carve AirServer.exe -o carved/
```

Every BLOB is a libsodium `crypto_secretbox` (XSalsa20-Poly1305), laid out
`nonce(24) || tag(16) || ciphertext`, under a key derived per database:

```
key = BLAKE2b-256(message="", key=PASS, salt=<the salt table>, person=PERSON)
    = crypto_generichash_blake2b_salt_personal

PERSON = <16 ASCII bytes>   ([S5](#validation))
PASS   = <64 ASCII bytes>   ([S6](#validation))
```

Both constants are plain literals in the shipped binary — in 5.7.2's `AirServer.exe`
at file offset `0x1549558` (`PERSON`) and `0x1549588` (`PASS`), adjacent, so
`strings | grep 'App Dynamic'` finds them. (Those offsets fall in `.data`, not
`.rdata` as this file used to say; `.rdata`'s raw range in that build ends at
`0x1521200`.) The **salt is per-database** and lives in the file's own `salt` table, so
the key must be derived from the file rather than hardcoded.

**Neither constant is checked in.** `src/airserver_db.rs` used to carry them as string
literals, which meant this repository redistributed App Dynamic's material;
`nix/airserver-carve.nix` now recovers them at build time from the pinned installer and
`cast-replay/build.rs` hands them to the crate. A build without them is a supported
state: `Kek::provisioned()` is `None` and the live AirServer path reports
`ReplayError::NoKek` rather than silently dropping the identity.

The carver hardcodes **no offsets**, because the offsets above are per-build — across
5.7.0/5.7.1/5.7.2 the database alone sits at `0xbc7284`, `0xbcb244` and `0xbcff24`.
Instead:

* the database is located by its own `SQLite format 3\0` header, sized from the page
  size at `+16` and page count at `+28`, and accepted only after a full
  `PRAGMA integrity_check` plus the schema the reader needs — the integrity check being
  the load-bearing gate, since a carve of the wrong length still parses its first page;
* the constants are located by candidate search anchored on `CompanyName` from the PE
  version resource, and confirmed by Poly1305 against the carved database. The anchor is
  *not* either constant (`App Dynamic ehf`, 15 bytes, against a 16-byte personalisation
  ending in a period) but narrows ~5 M candidate pairs to four. Because the oracle is an
  AEAD tag, a wrong pair cannot be emitted — the carve either produces the right answer
  or fails.

Verified against all three 5.7.x builds; `nix build .#airserver-carve` prints where the
database was found and how the constants were confirmed.

One exception worth knowing, because it fails confusingly: `metadata.json` is
declared `TEXT` and is **not** a secretbox. It is plaintext `{"generated": <unix>}`.

Export the fixtures:

```sh
airserver_castdb.py builtin_castdb_win5.7.2.sqlite \
    --export crates/cast-replay/fixtures/airserver
```

That writes the device certificate, both chain certificates, the peer key, the 1095
peer certificates at a fixed 738-byte stride, both signature sets, and
`airserver_manifest.json` (the layout constants `src/airserver.rs` asserts against).
It writes **no JWTs** — see below.

## 4. The AirServer remote pull

Reimplemented in `src/airserver_api.rs`. Recovered from **2025.7.23**'s
`AirServer.exe`; these are raw virtual addresses, not Ghidra-rebased.

The URL literal at `.rdata 0x140b81dc0` is wrapped into a `QUrl` global at
`0x1410be428`, whose only consumer is the request builder at `0x14012dc11`:

```
QNetworkRequest(QUrl @0x1410be428)
setTransferTimeout(0x36ee80)                     -> 3 600 000 ms
setHeader(ContentTypeHeader, "application/json")
setRawHeader("AD-Redirect-Supported", "1")
setRawHeader("AD-Db-Schema-Version",  "2")
setAttribute(25 RedirectPolicyAttribute, 1 NoLessSafeRedirectPolicy)
QVariant(QList<QVariant>) -> QJsonDocument::fromVariant -> toJson
```

giving:

```
POST https://api.airserver.com/cast_certificates/get
Content-Type: application/json
AD-Redirect-Supported: 1
AD-Db-Schema-Version: 2
body: []
```

`AD-` is App Dynamic. The schema version is a real compatibility assertion, not a
constant: the client probes for that generation at runtime with
`pragma_table_info('jwt_token_header') WHERE name = 'includes_chain'`.

**The one unrecovered part.** The body is a JSON *array* assembled in a loop at
`0x14012e100` over a `QList` the caller already holds; the element shape was never
recovered. An empty array is accepted and returns a complete, current credential
set, so `[]` is what we send — sufficient, not faithful. Decompiling that loop is
the way to fix it, and a future `AD-Db-Schema-Version` could start requiring an
element.

The response is a whole SQLite database (~14 MB) in the same encrypted format as
§3, holding one identity and ~30 rolling windows. Observed responses carry a
*different* SHIELD identity from the bundled one, so there is a pool or a rotation
behind the endpoint.

Also present, and **not** credentials:
`https://download.airserver.com/cast_certificates/config.json` is a
client-evaluated policy file whose matcher
`01-block-a-cast.db-that-was-generated-before-a-date` keys on `db_generated`, i.e.
the `metadata` field above — how App Dynamic forces clients off a stale database.

## The two test databases

`fixtures/airserver/db_trimmed.sqlite` and `db_trimmed_512.sqlite` are **not**
credentials the receiver uses; they are the corpus `src/airserver_db.rs` is tested
against offline. Full schema, the six tables the reader reads, three windows, and an
*empty* `jwt_token`. The 512-byte page size variant exists so the 778-byte
encrypted certificates span multiple pages, which the default 4096 never exercises.

Regenerate both from a full database:

```python
import os, sqlite3
src = "builtin_castdb_win5.7.2.sqlite"
for name, page_size in [("db_trimmed.sqlite", 4096), ("db_trimmed_512.sqlite", 512)]:
    if os.path.exists(name):
        os.remove(name)
    s, d = sqlite3.connect(src), sqlite3.connect(name)
    d.execute("pragma page_size=%d" % page_size)
    d.execute("vacuum")
    for (sql,) in s.execute(
        "select sql from sqlite_master where type='table' and sql is not null"
    ):
        d.execute(sql)
    for t in ["device_info", "device_cert_chain", "daily_private", "salt", "metadata"]:
        rows = s.execute("select * from " + t).fetchall()
        d.executemany(
            "insert into %s values (%s)" % (t, ",".join("?" * len(rows[0]))), rows
        )
    rows = s.execute("select * from daily_cert order by start_time limit 3").fetchall()
    d.executemany(
        "insert into daily_cert values (%s)" % ",".join("?" * len(rows[0])), rows
    )
    # jwt_token is deliberately left empty.
    d.commit()
    d.execute("vacuum")
    d.commit()
    d.close()
    s.close()
```

## What was deliberately not extracted

AirServer's database also holds **4380 pre-signed RS256 JWTs** (20 520 in a live
response). None are in this tree and nothing here reads them: `src/airserver_db.rs`
never touches `jwt_token`, and the exporter writes certificates, the peer key and
the signature sets only.

They are a different kind of object from a receiver-auth signature, which is inert
without the peer certificate it covers — a pre-signed JWT *is* the credential for
the length of its window. They serve outbound app identification, which this
project does not implement; **D42** records that decision and what the capability
was measured to be worth (one third-party app in Google's whole whitelist).

## Verifying a re-derivation

The checks that would catch a mistake, in increasing strength:

* `cargo test -p cast-replay` — re-verifies all 1800 CKS signatures against the
  re-issued certificates, samples the AirServer table, and asserts that a
  credential built through the *database* path is byte-identical to one built from
  the exported fixtures. That last one is cross-implementation agreement between
  this crate and the Python tool.
* `nix build .#checks.openscreen-device-auth` — openscreen's own sender-side
  verifier judges both chains: `cks-chain-google-roots` and
  `airserver-chain-google-roots` must both be `ok`, and both `*-nonce-echoed`
  controls must be `kCastV2SignedBlobsMismatch`.
* `openscreen_conformance.py` in the RE directory ran the same acceptance path over
  all 900 CKS windows and all 1095 AirServer ones, with the checker itself validated
  against fourteen negative controls.
