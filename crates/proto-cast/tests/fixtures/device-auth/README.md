# Device-auth vectors

What a Cast sender receives from this receiver when it challenges it, and what a real
sender implementation makes of it.

Each subdirectory is one case: the peer (TLS) certificate the sender sees, the
`DeviceAuthMessage` we answer the challenge with, the nonce the sender used, optionally a
trust anchor to evaluate the chain against, the wall-clock time to verify at, and the
verdict expected.

Two things consume these, and the pair is the point:

- `tests/device_auth_vectors.rs` regenerates every byte from the current source and
  compares. That is what stops the vectors from describing a receiver we no longer have.
- The `openscreen-device-auth` flake check compiles openscreen's own sender-side verifier
  — `cast/sender/channel/cast_auth_util.cc` plus the certificate path builder under
  `cast/common/certificate/`, the same code Chrome runs — and asserts each verdict. That
  is what stops "a sender would accept this" from being our opinion.

The keys are fixed and checked in for one reason: RSA PKCS#1 v1.5 signing is
deterministic, and X.509 issuance here is too, so fixed keys plus a fixed clock make the
whole vector byte-reproducible. **`dev-*-key.pem` and `tls-key.pem` are test keys and
nothing else.** They authenticate no device, and a real *device* credential — one whose
private key Google issued — must never appear here, in this repository, or anywhere
world-readable.

The `cks-*` vectors are the exception worth naming: they are generated from the peer key
in `crates/cast-replay/fixtures/`, which is real material extracted from a shipped binary.
It is not a device key — no software receiver holds one, which is the reason the replay
exists at all — and its provenance and caveats are recorded in that directory's README.

## Reading the verdicts

`expect` holds either `ok` or `error <code>`, where the code is openscreen's
`Error::Code` name. The interesting ones:

- `dev-chain-trusted` — **ok**. Everything about our auth response is right *except* whose
  root it chains to: told to trust our dev root, a real sender accepts it.
- `dev-chain-google-roots` — **kCastV2CertNotSignedByTrustedCa**. The same bytes against
  the trust store senders actually ship. This is the whole of why casting from Chrome
  does not work with a self-generated credential, stated as an executed result rather than
  a belief.
- `cks-chain-google-roots` — **ok**. The case above, flipped. Same receiver, same response
  shape, a `cast-replay` credential instead of a generated one — and against the roots senders
  actually ship, a real sender accepts it. This is the executed form of "Cast works now",
  and it is generated offline from the checked-in table at a fixed clock, so it needs
  neither the network nor the day it runs on.
- `cks-nonce-echoed` — **kCastV2SignedBlobsMismatch**. The negative control for the rule
  the whole replay rests on. The same credential and the same untouched signature, with the
  sender's nonce echoed back: the sender rebuilds `nonce || cert` and refuses it. This is
  why `NonceEcho` is a type rather than a comment — without this case, the one above
  passing would not establish that the empty echo is what carries it.
- `nonce-omitted`, `nonce-mismatched` — **ok**, both. The sender records a nonce mismatch
  and then ignores it: `VerifySenderNonce` defaults to not enforcing, and verification
  rebuilds the signed blob from the nonce the *response* echoed rather than the one it
  sent. This is the behaviour Shanocast is built on, and these two cases are what turn
  reading that code into knowing it.
- `nonce-not-covered` — **kCastV2SignedBlobsMismatch**. The negative control: a signature
  over the certificate alone while claiming to echo a nonce. Without this, the cases above
  passing would not tell us the signed-blob layout is being checked at all.
- `tls-cert-unbounded` — **kCastV2TlsCertValidityPeriodTooLong**. A peer certificate with
  rcgen's default 1975→4096 window, which is what this receiver used to present. Kept as a
  regression lock, because the failure it caused is invisible from either end.
