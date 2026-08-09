//! Legacy AirPlay pairing — `/pair-setup` and `/pair-verify` (feature bit 27).
//!
//! The *third* auth regime, and the one a mirroring receiver wants. The three are laid
//! out in `docs/airplay-research.md` §2; what selects this one is bit 27 set with every
//! HomeKit bit clear, and what distinguishes it on the wire is the **absence** of an
//! `X-Apple-HKP` header (HomeKit's `/pair-verify` accepts only `X-Apple-HKP: 3`).
//!
//! Why it exists here at all: pairing-less receivers are listed by *some* senders and,
//! on current iOS, apparently not by Screen Mirroring. UxPlay ships bit 27 **on** by
//! default and documents the off variant as an option, which is evidence about which
//! path stays exercised. This module is that path.
//!
//! ## What the flow actually is
//!
//! Nothing here is a password prompt — legacy "pairing" is a key exchange the sender
//! drives unattended, and no PIN is involved (that is `/pair-pin-start`, a different
//! and older thing this does not implement).
//!
//! - **`POST /pair-setup`** → our Ed25519 public key, 32 raw bytes. The sender keeps it
//!   as this receiver's identity.
//! - **`POST /pair-verify`, stage 1** — body `01 00 00 00 ‖ their X25519 pub (32) ‖
//!   their Ed25519 pub (32)`. We answer `our X25519 pub (32) ‖ AES-CTR(our signature
//!   over our pub ‖ their pub) (64)`.
//! - **`POST /pair-verify`, stage 2** — body `00 00 00 00 ‖ AES-CTR(their signature)
//!   (64)`, continuing the *same* keystream. We verify their signature over
//!   `their pub ‖ our pub` against the Ed25519 key from stage 1, and answer empty.
//!
//! The keystream being continuous across the two stages is the detail that bites: the
//! sender runs its cipher over our 64-byte reply before encrypting its own signature
//! (`pyatv/protocols/airplay/srp.py:verify2` does exactly this), so our decryption of
//! stage 2 must start at keystream offset 64, not 0.
//!
//! ## What it buys, beyond being listed
//!
//! The verified ECDH secret re-keys the media: with bit 27 set the audio AES key
//! becomes `SHA512(aeskey ‖ shared)[0..16]` instead of the unwrapped key itself
//! (research §4.3). Get that pairing wrong and a session completes cleanly and renders
//! noise — which is why [`PairVerify::shared_secret`] is handed to the SDP layer rather
//! than kept here.
//!
//! Sans-I/O and synchronous, like every other protocol core here (ground rule 3): bytes
//! in, bytes out, no sockets.

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use sha2::{Digest as _, Sha512};
use x25519_dalek::{EphemeralSecret, PublicKey as X25519Public, StaticSecret};

use crate::error::AirPlayError;

/// AES-128-CTR, the cipher the verify exchange encrypts its signatures with.
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Raw Ed25519/X25519 public keys are 32 bytes; Ed25519 signatures are 64.
const KEY_LEN: usize = 32;
const SIG_LEN: usize = 64;
/// `01 00 00 00 ‖ curve25519 ‖ ed25519`.
const VERIFY1_LEN: usize = 4 + KEY_LEN + KEY_LEN;
/// `00 00 00 00 ‖ encrypted signature`.
const VERIFY2_LEN: usize = 4 + SIG_LEN;

/// The receiver's long-term Ed25519 identity.
///
/// Derived from a stable seed (the pairing UUID), so the key a sender pins today is the
/// key it verifies against tomorrow. A random key per boot would make every restart
/// look like a different device to anything that remembers.
///
/// The advertised `pk` — mDNS TXT and `/info` — is **this key**, via
/// [`Self::public_key_hex`]: `advert.rs` calls into here rather than deriving its own.
/// It used to derive one (SHA-256 of the seed, where this key's seed is SHA-512), and
/// the two disagreed — a sender that pinned the advertisement then watched
/// `/pair-setup` hand over a different identity.
#[derive(Debug, Clone)]
pub struct PairingIdentity {
    signing: SigningKey,
}

impl PairingIdentity {
    /// Build the identity from a stable seed (the receiver's pairing UUID).
    #[must_use]
    pub fn from_seed(seed: &str) -> Self {
        let digest = Sha512::digest(seed.as_bytes());
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&digest[..KEY_LEN]);
        Self {
            signing: SigningKey::from_bytes(&bytes),
        }
    }

    /// The public half, as `/pair-setup` returns it and the `pk` TXT record advertises.
    #[must_use]
    pub fn public_key(&self) -> [u8; KEY_LEN] {
        self.signing.verifying_key().to_bytes()
    }

    /// The public half as the 64 lowercase hex chars the `pk` TXT records and the
    /// `/info` plist carry. Lives here so the advertisement and the pairing layer
    /// share one derivation and cannot diverge again.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        self.public_key()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The `/pair-setup` response body: our public key, raw.
    #[must_use]
    pub fn pair_setup_response(&self) -> Vec<u8> {
        self.public_key().to_vec()
    }
}

/// A `/pair-verify` exchange in progress.
///
/// Typestate by construction rather than by flag: [`Self::begin`] is the only way to
/// get one, and it exists only between the two stages. A stage-2 body arriving with no
/// stage 1 before it has no value of this type to be handed to, so "verify without a
/// handshake" is not reachable.
pub struct PairVerify {
    /// Our ephemeral X25519 public key, echoed in the stage-1 reply and signed.
    our_public: [u8; KEY_LEN],
    /// Their X25519 public key from stage 1.
    their_public: [u8; KEY_LEN],
    /// Their Ed25519 identity from stage 1, which stage 2's signature is checked with.
    their_identity: [u8; KEY_LEN],
    /// The ECDH secret — the thing the whole exchange exists to agree on.
    shared: [u8; KEY_LEN],
    /// The cipher, carried across both stages: the sender's keystream runs over our
    /// reply before its own signature, so ours must too.
    cipher: Aes128Ctr,
}

impl std::fmt::Debug for PairVerify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No key material in logs.
        f.debug_struct("PairVerify").finish_non_exhaustive()
    }
}

impl PairVerify {
    /// Stage 1: consume the sender's public keys, answer with ours and our signature.
    ///
    /// # Errors
    /// [`AirPlayError::Pairing`] if the body is not the expected 68 bytes or does not
    /// begin with the stage-1 marker.
    pub fn begin(identity: &PairingIdentity, body: &[u8]) -> Result<(Self, Vec<u8>), AirPlayError> {
        // Ephemeral at this boundary: the secret is made here, moved in, and consumed —
        // nothing can hold it across sessions. (`EphemeralSecret` used to enforce that
        // by type, and drawing it *inside* the state transition is what it cost: the
        // reply differs every run, so no captured iPhone transcript can ever be a
        // fixture for it, and the one part of the handshake where byte order is the
        // whole difficulty had only round-trip tests. #235; `proto-gamestream`'s
        // `PairingSeed` is the same call.)
        Self::begin_with(identity, body, StaticSecret::random())
    }

    /// [`Self::begin`] with the ephemeral secret supplied, so the reply is a pure
    /// function of its inputs and a wire capture can be checked in against it.
    ///
    /// # Errors
    /// As [`Self::begin`].
    pub fn begin_with(
        identity: &PairingIdentity,
        body: &[u8],
        secret: StaticSecret,
    ) -> Result<(Self, Vec<u8>), AirPlayError> {
        if body.len() < VERIFY1_LEN || body[0] != 0x01 {
            return Err(AirPlayError::Pairing(
                "pair-verify stage 1: expected 68 bytes beginning 01 00 00 00",
            ));
        }
        let mut their_public = [0u8; KEY_LEN];
        their_public.copy_from_slice(&body[4..4 + KEY_LEN]);
        let mut their_identity = [0u8; KEY_LEN];
        their_identity.copy_from_slice(&body[4 + KEY_LEN..VERIFY1_LEN]);

        let our_public = X25519Public::from(&secret).to_bytes();
        let shared = secret
            .diffie_hellman(&X25519Public::from(their_public))
            .to_bytes();

        let key = derive(b"Pair-Verify-AES-Key", &shared);
        let iv = derive(b"Pair-Verify-AES-IV", &shared);
        let mut cipher = Aes128Ctr::new(&key.into(), &iv.into());

        // We sign our public key followed by theirs; they sign the mirror image
        // (theirs, then ours) in stage 2. Getting the order backwards on either side
        // produces a signature that verifies against nothing.
        let mut signed_material = [0u8; KEY_LEN * 2];
        signed_material[..KEY_LEN].copy_from_slice(&our_public);
        signed_material[KEY_LEN..].copy_from_slice(&their_public);
        let mut signature = identity.signing.sign(&signed_material).to_bytes();
        cipher.apply_keystream(&mut signature);

        let mut reply = Vec::with_capacity(KEY_LEN + SIG_LEN);
        reply.extend_from_slice(&our_public);
        reply.extend_from_slice(&signature);

        Ok((
            Self {
                our_public,
                their_public,
                their_identity,
                shared,
                cipher,
            },
            reply,
        ))
    }

    /// Stage 2: check the sender's signature over the two public keys.
    ///
    /// Consumes the exchange either way — a failed verification is not something to
    /// retry against the same ephemeral key.
    ///
    /// # Errors
    /// [`AirPlayError::Pairing`] if the body is malformed, the sender's identity key is
    /// not a valid Ed25519 point, or the signature does not verify.
    pub fn finish(mut self, body: &[u8]) -> Result<[u8; KEY_LEN], AirPlayError> {
        if body.len() < VERIFY2_LEN || body[0] != 0x00 {
            return Err(AirPlayError::Pairing(
                "pair-verify stage 2: expected 68 bytes beginning 00 00 00 00",
            ));
        }
        let mut signature = [0u8; SIG_LEN];
        signature.copy_from_slice(&body[4..VERIFY2_LEN]);
        // The same cipher, continuing: the sender ran its keystream over our 64-byte
        // reply before encrypting this, so we are already at the matching offset.
        self.cipher.apply_keystream(&mut signature);

        let verifying = VerifyingKey::from_bytes(&self.their_identity)
            .map_err(|_| AirPlayError::Pairing("pair-verify: sender's identity key is invalid"))?;
        let mut signed_material = [0u8; KEY_LEN * 2];
        signed_material[..KEY_LEN].copy_from_slice(&self.their_public);
        signed_material[KEY_LEN..].copy_from_slice(&self.our_public);
        verifying
            .verify(&signed_material, &Signature::from_bytes(&signature))
            .map_err(|_| AirPlayError::Pairing("pair-verify: signature did not verify"))?;

        Ok(self.shared)
    }
}

/// `SHA512(label ‖ shared)[0..16]` — the verify exchange's key and IV derivation.
fn derive(label: &[u8], shared: &[u8; KEY_LEN]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label);
    hasher.update(shared);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Re-key an unwrapped media AES key with a verified pairing secret.
///
/// The half of bit 27 that is not about being listed: with legacy pairing on, the audio
/// key is `SHA512(aeskey ‖ shared)[0..16]` rather than the RSA-unwrapped key itself
/// (research §4.3). Mismatched, the session completes cleanly and renders noise, which
/// is the least debuggable failure in this protocol — so the type takes both halves at
/// once and there is no path that applies one without the other.
#[must_use]
pub fn rekey_media(aes_key: &[u8; 16], shared: &[u8; KEY_LEN]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(aes_key);
    hasher.update(shared);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_stage_one_reply_is_a_pure_function_of_its_inputs() {
        // With the ephemeral secret supplied, the 96-byte reply is deterministic — which
        // is what makes a captured iPhone transcript checkable in as a fixture at all
        // (#235). Until then this pins the current bytes: the signing order and the
        // keystream offset are exactly the regressions a round-trip test cannot see,
        // because both sides agreeing on the wrong thing round-trips fine. A pin is not
        // a validation (#200's distinction); replace the constant with the capture when
        // one lands.
        let identity = PairingIdentity::from_seed("test-receiver-uuid");
        let sender_secret = StaticSecret::from([0x21u8; KEY_LEN]);
        let sender_public = X25519Public::from(&sender_secret).to_bytes();
        let sender_signing = SigningKey::from_bytes(&[7u8; KEY_LEN]);
        let mut body = vec![0x01, 0x00, 0x00, 0x00];
        body.extend_from_slice(&sender_public);
        body.extend_from_slice(&sender_signing.verifying_key().to_bytes());

        let (_, reply) =
            PairVerify::begin_with(&identity, &body, StaticSecret::from([0x42u8; KEY_LEN]))
                .unwrap();
        let hex: String = reply.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "132c442be010fbd57e72603328aa76e71fccc1503aae219327d14d9c9993f472\
             d706bd215c5fcc87607aa8664e0c508c3f9dab5673cdd330b509d56c0e6de8d1\
             43894b78eb9e36e1762e5e84331be73db576cfd320482c94d1414dfa86943d0e"
        );

        // And the same inputs twice are the same bytes: nothing inside draws entropy.
        let (_, again) =
            PairVerify::begin_with(&identity, &body, StaticSecret::from([0x42u8; KEY_LEN]))
                .unwrap();
        assert_eq!(reply, again);
    }

    /// The sender's half, exactly as `pyatv/protocols/airplay/srp.py` performs it.
    ///
    /// Written from the reference *client* rather than guessed, because the two details
    /// that break this exchange — the signing order and the continuous keystream — are
    /// invisible in a one-sided implementation: both sides agreeing on the wrong thing
    /// is indistinguishable from both agreeing on the right one until a real iPhone
    /// arrives.
    struct Sender {
        signing: SigningKey,
        secret: Option<EphemeralSecret>,
        public: [u8; KEY_LEN],
    }

    impl Sender {
        fn new() -> Self {
            let signing = SigningKey::from_bytes(&[7u8; KEY_LEN]);
            let secret = EphemeralSecret::random();
            let public = X25519Public::from(&secret).to_bytes();
            Self {
                signing,
                secret: Some(secret),
                public,
            }
        }

        fn verify1(&self) -> Vec<u8> {
            let mut body = vec![0x01, 0x00, 0x00, 0x00];
            body.extend_from_slice(&self.public);
            body.extend_from_slice(&self.signing.verifying_key().to_bytes());
            body
        }

        fn verify2(&mut self, reply: &[u8]) -> Vec<u8> {
            let mut their_public = [0u8; KEY_LEN];
            their_public.copy_from_slice(&reply[..KEY_LEN]);
            let shared = self
                .secret
                .take()
                .unwrap()
                .diffie_hellman(&X25519Public::from(their_public))
                .to_bytes();

            let key = derive(b"Pair-Verify-AES-Key", &shared);
            let iv = derive(b"Pair-Verify-AES-IV", &shared);
            let mut cipher = Aes128Ctr::new(&key.into(), &iv.into());

            let mut signed_material = [0u8; KEY_LEN * 2];
            signed_material[..KEY_LEN].copy_from_slice(&self.public);
            signed_material[KEY_LEN..].copy_from_slice(&their_public);
            let mut signature = self.signing.sign(&signed_material).to_bytes();

            // The keystream runs over the receiver's signature first — this is the step
            // that makes the two ciphers line up, and skipping it is the classic break.
            let mut theirs = reply[KEY_LEN..].to_vec();
            cipher.apply_keystream(&mut theirs);
            cipher.apply_keystream(&mut signature);

            let mut body = vec![0x00, 0x00, 0x00, 0x00];
            body.extend_from_slice(&signature);
            body
        }
    }

    #[test]
    fn a_full_exchange_agrees_on_a_secret() {
        let identity = PairingIdentity::from_seed("castaway-test");
        let mut sender = Sender::new();
        let (verify, reply) = PairVerify::begin(&identity, &sender.verify1()).unwrap();
        assert_eq!(reply.len(), KEY_LEN + SIG_LEN);
        let body = sender.verify2(&reply);
        let shared = verify.finish(&body).unwrap();
        assert_ne!(shared, [0u8; KEY_LEN], "a zero secret means no exchange");
    }

    #[test]
    fn a_signature_over_the_wrong_material_is_refused() {
        // The failure this guards is the one that renders noise rather than erroring:
        // both sides complete, and the media key is derived from a secret only one of
        // them believes in.
        let identity = PairingIdentity::from_seed("castaway-test");
        let mut sender = Sender::new();
        let (verify, reply) = PairVerify::begin(&identity, &sender.verify1()).unwrap();
        let mut body = sender.verify2(&reply);
        body[10] ^= 0xff;
        assert!(matches!(
            verify.finish(&body),
            Err(AirPlayError::Pairing(_))
        ));
    }

    #[test]
    fn the_keystream_must_be_continuous_across_the_two_stages() {
        // A receiver that restarts its cipher for stage 2 decrypts garbage. Same
        // exchange, with the sender *not* advancing over our reply: it must fail.
        let identity = PairingIdentity::from_seed("castaway-test");
        let mut sender = Sender::new();
        let (verify, reply) = PairVerify::begin(&identity, &sender.verify1()).unwrap();

        let mut their_public = [0u8; KEY_LEN];
        their_public.copy_from_slice(&reply[..KEY_LEN]);
        let shared = sender
            .secret
            .take()
            .unwrap()
            .diffie_hellman(&X25519Public::from(their_public))
            .to_bytes();
        let key = derive(b"Pair-Verify-AES-Key", &shared);
        let iv = derive(b"Pair-Verify-AES-IV", &shared);
        let mut cipher = Aes128Ctr::new(&key.into(), &iv.into());
        let mut signed_material = [0u8; KEY_LEN * 2];
        signed_material[..KEY_LEN].copy_from_slice(&sender.public);
        signed_material[KEY_LEN..].copy_from_slice(&their_public);
        let mut signature = sender.signing.sign(&signed_material).to_bytes();
        cipher.apply_keystream(&mut signature); // …from offset 0. Wrong.
        let mut body = vec![0x00, 0x00, 0x00, 0x00];
        body.extend_from_slice(&signature);

        assert!(matches!(
            verify.finish(&body),
            Err(AirPlayError::Pairing(_))
        ));
    }

    #[test]
    fn the_identity_is_stable_across_restarts() {
        // A sender that pins this key must find the same one tomorrow.
        let a = PairingIdentity::from_seed("dma.space/screen");
        let b = PairingIdentity::from_seed("dma.space/screen");
        assert_eq!(a.public_key(), b.public_key());
        assert_ne!(
            a.public_key(),
            PairingIdentity::from_seed("other").public_key()
        );
        assert_eq!(a.pair_setup_response().len(), KEY_LEN);
    }

    #[test]
    fn a_truncated_or_mislabelled_body_is_an_error_not_a_panic() {
        // Every one of these is attacker-supplied length on a LAN-facing endpoint.
        let identity = PairingIdentity::from_seed("castaway-test");
        assert!(PairVerify::begin(&identity, &[]).is_err());
        assert!(PairVerify::begin(&identity, &[0x01; 8]).is_err());
        // Right length, wrong marker: this is a stage-2 body arriving first.
        assert!(PairVerify::begin(&identity, &[0x00; VERIFY1_LEN]).is_err());

        let sender = Sender::new();
        let (verify, _) = PairVerify::begin(&identity, &sender.verify1()).unwrap();
        assert!(verify.finish(&[0x00; 4]).is_err());
    }

    #[test]
    fn the_media_key_is_rehashed_with_the_pairing_secret() {
        // Bit 27's other half (research §4.3): the audio key is not the unwrapped key
        // when pairing is on, and a mismatch here renders noise rather than failing.
        let key = [0x11u8; 16];
        let shared = [0x22u8; KEY_LEN];
        let rekeyed = rekey_media(&key, &shared);
        assert_ne!(rekeyed, key);
        assert_eq!(rekeyed, rekey_media(&key, &shared), "must be deterministic");
        assert_ne!(rekeyed, rekey_media(&key, &[0x23u8; KEY_LEN]));
    }
}
