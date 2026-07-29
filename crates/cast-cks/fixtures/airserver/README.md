# AirServer fixtures

The second offline Cast receiver-auth identity. Consumed by
`src/airserver.rs`; the sibling `../` directory holds the CKS (AirReceiver) one.

## Provenance

Extracted from AirServer 5.7.2's bundled credential database (***REMOVED: App Dynamic BLAKE2b personalisation, PROVENANCE S5***),
which ships inside the installer as an encrypted SQLite file. The 2025.7.23 build
no longer carries it as a loose file but links it into `AirServer.exe`; the copy
carved from that binary is byte-identical to the 5.7.2 one.

Recovered with the tooling and notes in
`re-shell/artifacts/airreceiver-cast-signatures/` —
`airserver_castdb.py` (blob decryption, verification, export) and
`AIRSERVER_HANDOFF.md` (how to run it cold). Every BLOB in that database is
libsodium `crypto_secretbox` (XSalsa20-Poly1305) under a BLAKE2b-derived key
whose two constants are literals in the shipped binary.

Landed here as **fixtures, not a dependency** — ground rule 9. Nothing in this
tree links, ships or shells out to AirServer.

Regenerate with:

```sh
airserver_castdb.py <cast.db> --export crates/cast-cks/fixtures/airserver
```

## What is *not* here

The database also carries **4380 pre-signed RS256 JWTs**. None of them are in
this directory and none are used by this crate. They serve *outbound* app
identification, which castaway does not implement — see **D42** for the decision
and for what that capability was measured to be worth. The export deliberately
writes certificates, the peer key and the signature sets only.

The distinction matters operationally: a receiver-auth signature is inert without
the peer certificate it covers, whereas a pre-signed JWT is itself a usable
bearer credential for the length of its window. Only the former is here.

## The identity

```
device      CN=2001805200936810051,OU=Widevine,O=Google Inc,L=Kirkland,ST=Washington,C=US
issuer      CN=NVidia mdarcy NVidia Tegra X1 T210 Cast ICA
chain[1]    CN=Widevine Cast Subroot
```

An NVIDIA SHIELD leaf reached through the Widevine-backed provisioning path
(`ClientAuthCredsWidevine`), rather than a Eureka ICA leaf like the CKS identity.
Openscreen accepts it: 1095/1095 windows pass the sender-side acceptance path.
That independence is the entire reason to carry a second identity — see the
module docs on `src/airserver.rs`.

| File | Contents |
|---|---|
| `airserver_device_crt.der` | the Google device certificate — `AuthResponse.client_auth_certificate` |
| `airserver_chain0.der` | `NVidia mdarcy … Cast ICA` |
| `airserver_chain1.der` | `Widevine Cast Subroot` |
| `airserver_peer_key.der` | the peer RSA-2048 private key, PKCS#1 DER. One key for every window. |
| `airserver_peer_certs.bin` | 1095 peer certificates, 738 bytes each at a fixed stride, window order |
| `airserver_sha1.bin` | 1095 × 256-byte SHA-1 device signatures, window order |
| `airserver_sha256.bin` | 1095 × 256-byte SHA-256 device signatures, window order |
| `airserver_manifest.json` | the layout constants `src/airserver.rs` asserts against |

## Coverage, and how it differs from CKS

1095 windows, **2024-03-20 → 2027-03-21**, all signatures verified by the
exporter against the device certificate's own public key (1095/1095 for both
SHA-1 and SHA-256).

Two structural differences from the CKS table, both load-bearing:

* **Windows overlap.** Windows step **1 day** but are valid for **2 days**, so at
  any instant two are valid. CKS steps 2 days with 2-day validity and tiles.
  `index_at` returns the later window, which has more life left.
* **The certificates cannot be re-issued from a template.** CKS's per-window
  certificates differ only in validity, so the crate rebuilds them from one
  template and one key. AirServer's also differ in serial (linear in the index)
  and in **subject CN, which is a fresh random UUID per window**. That UUID is
  not derivable, and the device signature covers the certificate's exact DER, so
  a rebuilt certificate would be rejected. Hence 790 KiB of certificates checked
  in verbatim rather than a template.

## Caveats

* **This identity is borrowed**, exactly like the CKS one, and shared with every
  install of AirServer. Google can revoke it. D41's cost statement applies
  unchanged, and carrying two identities mitigates the *consequence* of a
  revocation without reducing its likelihood.
* **It expires eight months before the CKS table.** It is not a horizon
  improvement and is not the default. See `src/provider.rs` on why the order is
  operator policy.
* **The live endpoint is not used.** `api.airserver.com/cast_certificates/get`
  vends a rolling 30-window database under a *different* SHIELD identity, so
  there is a pool or rotation behind it. The crate deliberately does not call it:
  an unattended panel refreshing on a schedule is precisely the "do not run this
  in a loop" case that `AIRSERVER_HANDOFF.md` warns about, and the offline table
  plus the CKS backend already cover every window either can. Adding it would be
  a new network path with its own failure modes for no coverage we lack.
