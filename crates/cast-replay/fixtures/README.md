# CKS fixtures

The offline half of `cast-replay`: one Cast receiver identity and a table of
precomputed receiver-auth signatures covering it.

## Provenance

Extracted from `libAirReceiver.so` 5.1.7 (arm64-v8a, `com.softmedia.receiver.lite`),
where the table ships inside an AES-128-CTR `dbio` container. Recovered with the
tooling and notes in `re-shell/artifacts/airreceiver-cast-signatures/`
(`dbio.py`, `cks_store.py`); the container format, the key derivation and the
on-device re-issue are documented in `CKS_5.1.7.md` there.

**No longer checked in.** The device certificate, its intermediate, the peer
template and key, and the 900 windows of signatures are SoftMedia's identity — a
Google-issued Cast device credential with an RSA private key — so they are carved
out of `libAirReceiver.so` at build time by `nix/airreceiver-carve.nix` and embedded
by `cast-replay/build.rs`. A build without the carve has no bundled CKS identity at
all (`ReplayError::NoIdentity`). What the carve produces is byte-identical to what
used to live here, which is how the change was verified.

The same is true of `src/api.rs`'s two backend constants, which are carved from the
same library rather than written down.

Landed as **fixtures, not a dependency** — ground rule 9. Nothing in this tree links,
ships or shells out to AirReceiver; what we keep is a reimplementation of the
derivation that makes the carved bytes usable.

Reproduce the carved half with `nix build .#airreceiver-carve`.

**To re-derive these from scratch, see [`../PROVENANCE.md`](../PROVENANCE.md) §1**,
which carries the source-artifact hashes, the APK and ABI they came from, the `dbio`
container offsets, the KEK recovery, and the commands. §2 covers the backend request
`src/api.rs` reimplements. The tooling referenced above is **not** under version
control, so that file is the durable record rather than a pointer to one.

The first six names below are what the carve writes into its output directory; only
`pinned_roots.pem` and `roots/` are files in this repository.

| File | Contents |
|---|---|
| `device_cert.pem` | Google device certificate `CN=RYW0O FA8FCA6AC5A0`, valid 2014-03-07 → 2034-03-02. `AuthResponse.client_auth_certificate`. |
| `ica.pem` | `CN=Eureka Gen1 ICA`, which chains it to `Eureka Root CA`. `AuthResponse.intermediate_certificate`. |
| `peer_template.der` | The peer certificate, 734 bytes, carrying window 0's validity. Re-issued per window by `template.rs`. |
| `peer_key.der` | Its RSA-2048 private key, PKCS#1 DER. Self-signs each re-issued certificate. |
| `signatures_sha1.bin` | 900 × 256-byte SHA-1 signatures, window order. |
| `signatures_sha256.bin` | 900 × 256-byte SHA-256 signatures, window order. |
| `pinned_roots.pem` | Starfield Class 2 and Amazon Root CA 1 — the two roots the reference client pins for `cast.remotetogo.com`, in place of the system store. |
| `roots/cast_root_ca.der` | `CN=Cast Root CA`, serial 2, valid to 2034-03-28. Anchors the **AirServer** chain. |
| `roots/eureka_root_ca.der` | `CN=Eureka Root CA`, serial 1, valid to 2032-12-12. Anchors the **CKS** chain. |

`roots/` is the odd one out and is not RE material: those two are the whole of
Chromium's Cast trust store, public and static, taken verbatim from openscreen's
`cast/common/certificate/{cast_root_ca_cert_der,eureka_root_ca_der}-inc.h`. They
are here only so a CRL check covers the same chain the sender's does — what we
present tops out one certificate *below* the root, and a revocation keyed on the
anchor would otherwise be invisible to us and fatal to a Chromium sender (#123).
`src/roots.rs` says why at length; `roots::tests` pins each one to the identity it
anchors.

## Coverage

900 two-day windows, **2023-01-01 → 2027-12-06**. Window *n* starts at
`1672531200 + n * 172800`. All 1800 signatures verify against `device_cert.pem`
over the certificate `template.rs` re-issues, and all 1800 are accepted by
Openscreen's sender-side path (chain, policy, validity, digest, signature) —
`openscreen_conformance.py` in the RE directory runs that, with the checker
validated against fourteen negative controls.

`table.rs` re-verifies all 1800 on every test run, so a regression in the
re-issue shows up as a test failure rather than as senders quietly refusing to
connect.

## Caveats — read before relying on this

* **It expires.** The table stops on **2027-12-06**. After that only the network
  path can produce a credential, and `CksTable::credential_at` returns
  `ReplayError::OutOfRange` rather than something that looks like it works.
* **It is not our identity.** This is AirReceiver's Cast identity, shared with
  every install of that app and with `com.softmedia.receiver.castapp`. We are one
  more holder of it, not the holder.
* **It is revocable.** `AuthResponse` carries a `crl` field and Chrome fetches the
  Cast device CRL. Google can revoke `RYW0O FA8FCA6AC5A0` at any time and this
  stops working before 2027 — nothing here can detect that in advance. Openscreen
  senders default to `kCrlOptional`, so returning no CRL is accepted today.
* **The chain's own ceiling is 2032-12-12**, set by `Eureka Root CA` expiring —
  earlier than the device certificate's own 2034 `notAfter`, and five years after
  the table runs out either way.
* **No device private key is involved.** None is present in any examined build,
  and the replay's structure is evidence the vendor holds none either: a party
  who could sign the nonce would have no reason to ship a 900-entry table. See
  `EXPIRY.md` in the RE directory.

## Regenerating

```
$ python3 cks_store.py libAirReceiver.so --openscreen --export out/
```

then copy `cks517_device_crt.pem` → `device_cert.pem`, `cks517_ica.pem` → `ica.pem`,
`cks517_peer_tmpl.der` → `peer_template.der`, `cks517_peer_key.der` → `peer_key.der`,
and the two `signatures_5.1.7_*.bin` files. If the window count or epoch moves,
update `EPOCH_UNIX`/`WINDOW_COUNT` in `src/table.rs` and re-bless the golden
certificate digests in its tests.
