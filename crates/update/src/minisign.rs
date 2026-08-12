//! Verifying minisign signatures — the receiver's half of the format.
//!
//! Only verification lives here. Signing is CI's job and CI has the real tool
//! (`.github/workflows/release.yml`), which is deliberate: the format was chosen so
//! standard tooling can inspect a release without this crate, and a second signer would
//! be a second thing that can disagree with it.
//!
//! The format, as minisign 0.12 writes it. A public key file:
//!
//! ```text
//! untrusted comment: minisign public key 95CCAF3DFCF194BE
//! RWS+lPH8Pa/MlVGWhAYSQ2ttBx8J7dNRK8MtmYkuiYJCXOXaFMM2fThK
//! ```
//!
//! whose base64 payload is `"Ed" ‖ key_id[8] ‖ public_key[32]`. A signature file:
//!
//! ```text
//! untrusted comment: castaway release manifest
//! <base64: alg[2] ‖ key_id[8] ‖ signature[64]>
//! trusted comment: castaway build 935 ae2f19e…
//! <base64: global_signature[64]>
//! ```
//!
//! Two signatures, and both are checked here. The first covers the message; the second
//! covers `signature ‖ trusted_comment`, which is what makes the trusted comment a
//! statement by the signer rather than a line anyone can rewrite. A verifier that skips
//! it agrees with `minisign -V` on every honest input and disagrees on exactly the
//! attack the trusted comment exists to stop, so it is not an optimisation worth having.
//!
//! Both algorithm tags are accepted. `Ed` signs the message; `ED` signs its BLAKE2b-512
//! digest and is what minisign has emitted by default since 0.10. Accepting only one
//! would be an undiagnosable break the day the CI runner's minisign moves.

use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, Verifier as _, VerifyingKey};
use thiserror::Error;

/// `alg[2] ‖ key_id[8] ‖ public_key[32]`.
const PUBLIC_KEY_LEN: usize = 42;
/// `alg[2] ‖ key_id[8] ‖ signature[64]`.
const SIGNATURE_LEN: usize = 74;
/// A bare Ed25519 signature.
const ED25519_LEN: usize = 64;
/// Where the key id starts in both payloads.
const KEY_ID_AT: usize = 2;
/// Where the key or signature starts in both payloads.
const BODY_AT: usize = 10;

/// A signing key's 8-byte identifier, as it appears in both files.
///
/// Compared as bytes rather than rendered: the hex in the untrusted comment is the same
/// number in the other byte order, and a mismatch between those two spellings is not
/// something a receiver should have an opinion about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyId([u8; 8]);

/// What a signature covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// `Ed` — Ed25519 over the message bytes.
    Legacy,
    /// `ED` — Ed25519 over BLAKE2b-512 of the message. minisign's default.
    Prehashed,
}

impl Algorithm {
    const fn from_tag(tag: [u8; 2]) -> Option<Self> {
        match &tag {
            b"Ed" => Some(Self::Legacy),
            b"ED" => Some(Self::Prehashed),
            _ => None,
        }
    }
}

/// A parsed minisign public key.
#[derive(Debug, Clone)]
pub struct PublicKey {
    key_id: KeyId,
    key: VerifyingKey,
}

/// A parsed minisign signature file.
#[derive(Debug, Clone)]
pub struct Signature {
    algorithm: Algorithm,
    key_id: KeyId,
    signature: [u8; ED25519_LEN],
    trusted_comment: String,
    global_signature: [u8; ED25519_LEN],
}

/// Everything that can go wrong reading or checking a signature.
#[derive(Debug, Error)]
pub enum SignatureError {
    /// The file held comments and nothing else — the shape `release-key.pub` has before
    /// anyone has run the keygen. Distinguished from a corrupt key on purpose: one is a
    /// build that was never given a key, the other is a build whose key was damaged, and
    /// only the first is an ordinary state.
    #[error("no key: the file carries comments but no payload line")]
    NoKey,
    /// A line that should have been base64 was not.
    #[error("{what}: not base64")]
    NotBase64 {
        /// Which line.
        what: &'static str,
        /// What the decoder said.
        #[source]
        source: base64::DecodeError,
    },
    /// A payload decoded, at the wrong length.
    #[error("{what}: expected {expected} bytes, got {actual}")]
    WrongLength {
        /// Which payload.
        what: &'static str,
        /// The length the format fixes.
        expected: usize,
        /// What was there.
        actual: usize,
    },
    /// The signature file ended before its four lines did.
    #[error("truncated signature file: no {what}")]
    Truncated {
        /// The line that was missing.
        what: &'static str,
    },
    /// A two-byte algorithm tag that is neither `Ed` nor `ED`.
    #[error("unknown signature algorithm {0:?}")]
    UnknownAlgorithm([u8; 2]),
    /// The 32 bytes are not a point on the curve.
    #[error("the public key is not a valid Ed25519 key")]
    MalformedKey,
    /// The signature was made by a different key than the one embedded in this build.
    /// The interesting case is a release signed with a key that was rotated without the
    /// panel being updated — which should stand down, not install.
    #[error("signed by key {signed_by:?}, but this build trusts {trusts:?}")]
    WrongKey {
        /// The key id in the signature file.
        signed_by: KeyId,
        /// The key id this build carries.
        trusts: KeyId,
    },
    /// The signature does not cover these bytes under this key.
    #[error("signature does not verify")]
    BadSignature,
    /// The signature verifies but the trusted comment was rewritten under it.
    #[error("the trusted comment's signature does not verify")]
    BadTrustedComment,
}

/// The `untrusted comment:` prefix minisign writes, and the only line kind skipped when
/// looking for a payload.
const UNTRUSTED_PREFIX: &str = "untrusted comment:";
/// The `trusted comment:` prefix, whose *content* is covered by the global signature.
const TRUSTED_PREFIX: &str = "trusted comment:";

impl PublicKey {
    /// Parse a minisign public key file.
    ///
    /// # Errors
    /// [`SignatureError::NoKey`] if the file has no payload line at all, and the
    /// decoding variants for a payload that is there and wrong.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
        let line = text
            .lines()
            .find(|l| !is_skippable(l))
            .ok_or(SignatureError::NoKey)?;
        let raw = fixed::<PUBLIC_KEY_LEN>(&decode(line, "public key")?, "public key")?;
        // The tag is checked even though only the *signature's* tag decides what is
        // hashed: a public key file whose first two bytes are not `Ed` is not a minisign
        // public key, and saying so here beats a signature mismatch later.
        let tag = [raw[0], raw[1]];
        if tag != *b"Ed" {
            return Err(SignatureError::UnknownAlgorithm(tag));
        }
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&raw[KEY_ID_AT..BODY_AT]);
        let mut key = [0u8; 32];
        key.copy_from_slice(&raw[BODY_AT..]);
        Ok(Self {
            key_id: KeyId(key_id),
            key: VerifyingKey::from_bytes(&key).map_err(|_| SignatureError::MalformedKey)?,
        })
    }

    /// This key's identifier, as it appears in a signature made by its secret half.
    #[must_use]
    pub const fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// Check `signature` against `message`, returning the trusted comment it carries.
    ///
    /// The trusted comment is returned rather than ignored because it is the only part
    /// of a minisign signature that says anything, and a caller that logs it says which
    /// release it just accepted in the signer's own words.
    ///
    /// # Errors
    /// [`SignatureError::WrongKey`] if the signature names another key,
    /// [`SignatureError::BadSignature`] if it does not cover `message`, and
    /// [`SignatureError::BadTrustedComment`] if the comment was rewritten under it.
    pub fn verify<'s>(
        &self,
        message: &[u8],
        signature: &'s Signature,
    ) -> Result<&'s str, SignatureError> {
        if signature.key_id != self.key_id {
            return Err(SignatureError::WrongKey {
                signed_by: signature.key_id,
                trusts: self.key_id,
            });
        }
        let prehashed;
        let signed: &[u8] = match signature.algorithm {
            Algorithm::Legacy => message,
            Algorithm::Prehashed => {
                prehashed = blake2b_simd::Params::new().hash_length(64).hash(message);
                prehashed.as_bytes()
            }
        };
        self.key
            .verify(signed, &EdSignature::from_bytes(&signature.signature))
            .map_err(|_| SignatureError::BadSignature)?;

        // The global signature covers the signature bytes followed by the trusted
        // comment's text — not the whole line, and with no separator between the two.
        let mut global = Vec::with_capacity(ED25519_LEN + signature.trusted_comment.len());
        global.extend_from_slice(&signature.signature);
        global.extend_from_slice(signature.trusted_comment.as_bytes());
        self.key
            .verify(
                &global,
                &EdSignature::from_bytes(&signature.global_signature),
            )
            .map_err(|_| SignatureError::BadTrustedComment)?;

        Ok(&signature.trusted_comment)
    }
}

impl Signature {
    /// Parse a minisign `.minisig` file.
    ///
    /// # Errors
    /// [`SignatureError::Truncated`] if a line is missing, plus the decoding variants.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
        let mut lines = text.lines();
        let sig_line = lines
            .find(|l| !is_skippable(l))
            .ok_or(SignatureError::Truncated { what: "signature" })?;
        let raw = fixed::<SIGNATURE_LEN>(&decode(sig_line, "signature")?, "signature")?;
        let tag = [raw[0], raw[1]];
        let algorithm = Algorithm::from_tag(tag).ok_or(SignatureError::UnknownAlgorithm(tag))?;
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&raw[KEY_ID_AT..BODY_AT]);
        let mut signature = [0u8; ED25519_LEN];
        signature.copy_from_slice(&raw[BODY_AT..]);

        // The trusted comment is required, not optional: without it there is nothing for
        // the global signature to cover, and minisign itself refuses such a file.
        let trusted_comment = lines
            .find_map(|l| l.trim().strip_prefix(TRUSTED_PREFIX))
            .map(|c| c.trim().to_owned())
            .ok_or(SignatureError::Truncated {
                what: "trusted comment",
            })?;
        let global_line = lines
            .find(|l| !is_skippable(l))
            .ok_or(SignatureError::Truncated {
                what: "trusted comment signature",
            })?;
        let global_signature = fixed::<ED25519_LEN>(
            &decode(global_line, "trusted comment signature")?,
            "trusted comment signature",
        )?;

        Ok(Self {
            algorithm,
            key_id: KeyId(key_id),
            signature,
            trusted_comment,
            global_signature,
        })
    }

    /// Which key made this signature.
    #[must_use]
    pub const fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// What this signature covers.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// The signer's own words about what this signature is for. Only meaningful after
    /// [`PublicKey::verify`] has returned it — before that it is attacker-controlled
    /// text, which is why the borrow the caller logs comes back *out* of `verify`.
    #[must_use]
    pub fn trusted_comment_unverified(&self) -> &str {
        &self.trusted_comment
    }
}

/// Blank lines and untrusted comments carry nothing and are skipped wherever they appear.
/// `trusted comment:` is *not* skippable — it is read positionally, because its text is
/// signed and a stray one would change what the global signature covers.
fn is_skippable(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with(UNTRUSTED_PREFIX)
}

fn decode(line: &str, what: &'static str) -> Result<Vec<u8>, SignatureError> {
    base64::engine::general_purpose::STANDARD
        .decode(line.trim())
        .map_err(|source| SignatureError::NotBase64 { what, source })
}

/// A decoded payload at exactly the length the format fixes for it.
fn fixed<const N: usize>(raw: &[u8], what: &'static str) -> Result<[u8; N], SignatureError> {
    <[u8; N]>::try_from(raw).map_err(|_| SignatureError::WrongLength {
        what,
        expected: N,
        actual: raw.len(),
    })
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::{Algorithm, PublicKey, Signature, SignatureError};

    /// The test key pair, generated by minisign 0.12 and checked in whole — the secret
    /// half included, because it signs fixtures and nothing else, and because
    /// `checks.release-manifest` needs it to reproduce the signature the release script
    /// makes. Regenerating it is `minisign -G -W -p test-release.pub -s test-release.key`
    /// in that directory, followed by that check's own instructions for the fixtures.
    const TEST_PUB: &str = include_str!("../fixtures/test-release.pub");
    const MANIFEST: &[u8] = include_bytes!("../fixtures/manifest.json");
    const MANIFEST_SIG: &str = include_str!("../fixtures/manifest.json.minisig");

    #[test]
    fn a_signature_minisign_made_verifies_against_the_key_minisign_made() {
        let key = PublicKey::parse(TEST_PUB).expect("the fixture key parses");
        let sig = Signature::parse(MANIFEST_SIG).expect("the fixture signature parses");
        // minisign 0.12 prehashes by default; asserting it here is what makes a future
        // toolchain that stops doing so a failure with a name rather than a mismatch.
        assert_eq!(sig.algorithm(), Algorithm::Prehashed);
        assert_eq!(sig.key_id(), key.key_id());
        let trusted = key.verify(MANIFEST, &sig).expect("the fixture verifies");
        assert!(trusted.starts_with("castaway build 935 "), "{trusted}");
    }

    #[test]
    fn one_flipped_byte_in_the_message_is_refused() {
        let key = PublicKey::parse(TEST_PUB).expect("key");
        let sig = Signature::parse(MANIFEST_SIG).expect("sig");
        let mut tampered = MANIFEST.to_vec();
        // The last digit of the build number: the smallest edit that changes what the
        // manifest claims, and the one an attacker actually wants.
        let at = tampered
            .windows(4)
            .position(|w| w == b"935,")
            .expect("the fixture carries build 935");
        tampered[at + 2] = b'6';
        assert!(matches!(
            key.verify(&tampered, &sig),
            Err(SignatureError::BadSignature)
        ));
    }

    #[test]
    fn a_rewritten_trusted_comment_is_refused_even_though_the_message_still_verifies() {
        let key = PublicKey::parse(TEST_PUB).expect("key");
        let rewritten = MANIFEST_SIG.replace("castaway build 935", "castaway build 9350");
        let sig = Signature::parse(&rewritten).expect("sig");
        assert!(matches!(
            key.verify(MANIFEST, &sig),
            Err(SignatureError::BadTrustedComment)
        ));
    }

    #[test]
    fn another_keys_signature_is_refused_by_key_id_before_the_curve_is_touched() {
        // The fixture key with one byte of its *key id* flipped: still a well-formed
        // minisign public key, and by the format's own rule a different key.
        let mut raw = base64::engine::general_purpose::STANDARD
            .decode(TEST_PUB.lines().nth(1).expect("payload line").trim())
            .expect("fixture base64");
        raw[2] ^= 0xff;
        let other = format!(
            "untrusted comment: another key\n{}\n",
            base64::engine::general_purpose::STANDARD.encode(&raw)
        );
        let key = PublicKey::parse(&other).expect("still a well-formed key");
        let sig = Signature::parse(MANIFEST_SIG).expect("sig");
        assert!(matches!(
            key.verify(MANIFEST, &sig),
            Err(SignatureError::WrongKey { .. })
        ));
    }

    #[test]
    fn a_comment_only_file_is_no_key_rather_than_a_parse_failure() {
        let stub = "untrusted comment: no release signing key has been generated yet\n";
        assert!(matches!(PublicKey::parse(stub), Err(SignatureError::NoKey)));
    }

    #[test]
    fn a_signature_file_without_its_trusted_comment_is_truncated() {
        let first_two: String = MANIFEST_SIG
            .lines()
            .take(2)
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(matches!(
            Signature::parse(&first_two),
            Err(SignatureError::Truncated {
                what: "trusted comment"
            })
        ));
    }
}
