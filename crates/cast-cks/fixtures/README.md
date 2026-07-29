# CKS fixtures

The offline half of `cast-cks`: one Cast receiver identity and a table of
precomputed receiver-auth signatures covering it.

## Provenance

Extracted from `libAirReceiver.so` 5.1.7 (arm64-v8a, `com.softmedia.receiver.lite`),
where the table ships inside an AES-128-CTR `dbio` container. Recovered with the
tooling and notes in `re-shell/artifacts/airreceiver-cast-signatures/`
(`dbio.py`, `cks_store.py`); the container format, the key derivation and the
on-device re-issue are documented in `CKS_5.1.7.md` there.

Landed here as **fixtures, not a dependency** — ground rule 9. Nothing in this
tree links, ships or shells out to AirReceiver; these are bytes plus a
reimplementation of the derivation that makes them usable.

| File | Contents |
|---|---|
| `device_cert.pem` | Google device certificate `CN=RYW0O FA8FCA6AC5A0`, valid 2014-03-07 → 2034-03-02. `AuthResponse.client_auth_certificate`. |
| `ica.pem` | `CN=Eureka Gen1 ICA`, which chains it to `Eureka Root CA`. `AuthResponse.intermediate_certificate`. |
| `peer_template.der` | The peer certificate, 734 bytes, carrying window 0's validity. Re-issued per window by `template.rs`. |
| `peer_key.der` | Its RSA-2048 private key, PKCS#1 DER. Self-signs each re-issued certificate. |
| `signatures_sha1.bin` | 900 × 256-byte SHA-1 signatures, window order. |
| `signatures_sha256.bin` | 900 × 256-byte SHA-256 signatures, window order. |
| `pinned_roots.pem` | Starfield Class 2 and Amazon Root CA 1 — the two roots the reference client pins for `cast.remotetogo.com`, in place of the system store. |

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
  path can produce a credential, and `StaticTable::credential_at` returns
  `CksError::OutOfRange` rather than something that looks like it works.
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
