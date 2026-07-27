//! # crypto-raop
//!
//! The two RSA operations AirPlay 1 needs, and nothing else.
//!
//! Both use the same key: the RSA private key of the original AirPort Express. It is
//! quarantined in this crate rather than spread through `proto-airplay` so there is
//! exactly one place to look for it, and exactly one place to change if it ever needs
//! to become configurable.
//!
//! ## About the key
//!
//! It is not a secret. It was extracted from AirPort Express firmware in 2011 and has
//! been carried, verbatim, by every open-source RAOP receiver since — shairport-sync,
//! UxPlay, RPiPlay, airplay2-receiver, owntone. Apple has never rotated it, because
//! doing so would break every AirPlay 1 sender in the field, and every one of those
//! senders still encrypts to the matching public key. It functions here as an
//! interoperability constant: without it a receiver cannot read a session key an iPhone
//! wrapped for it, and `et=1` cannot be advertised honestly.
//!
//! It is worth being clear about what it is not. It is not a device identity we present
//! to anyone, it signs nothing that authenticates *us*, and it grants no access to
//! anything of Apple's. The `Apple-Challenge` signature below is a sender's check that
//! it is talking to something that speaks AirPlay, not a trust decision.
//!
//! ## What this crate does not do
//!
//! FairPlay. `et=3`/`et=5` wrap the session key with FairPlay-SAP instead of RSA, and
//! unwrapping that needs `crypto-fairplay`'s unfinished derivation. This crate handles
//! `et=1` only.
#![forbid(unsafe_code)]

use std::net::IpAddr;
use std::sync::OnceLock;

use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::{Oaep, Pkcs1v15Sign, RsaPrivateKey};
use thiserror::Error;

/// Failures in the RAOP RSA operations.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RaopCryptoError {
    /// The embedded key would not parse. Only reachable if this crate is mis-built.
    #[error("the embedded AirPort key could not be loaded")]
    KeyUnavailable,

    /// The wrapped key would not decrypt with this private key — almost always because
    /// the sender wrapped it with FairPlay (`et=3`/`et=5`) rather than RSA (`et=1`).
    #[error("could not unwrap the session key (was it wrapped with FairPlay?)")]
    Unwrap,

    /// The unwrapped value was not a 128-bit AES key.
    #[error("unwrapped session key is {0} bytes, expected 16")]
    WrongKeyLength(usize),

    /// The challenge a sender sent was longer than the protocol allows.
    #[error("Apple-Challenge is {0} bytes, expected at most 16")]
    ChallengeTooLong(usize),

    /// The signing operation itself failed.
    #[error("signing the Apple-Challenge failed")]
    Sign,
}

/// The AirPort Express private key, PEM-encoded.
const AIRPORT_PEM: &str = include_str!("airport.pem");

/// An AES-128 session key is 16 bytes.
const AES_KEY_LEN: usize = 16;

/// A sender's challenge is at most 16 bytes.
const MAX_CHALLENGE_LEN: usize = 16;

/// The signed challenge buffer is zero-padded to at least this length before signing.
const CHALLENGE_PAD_TO: usize = 0x20;

/// Parse the embedded key once.
fn airport_key() -> Result<&'static RsaPrivateKey, RaopCryptoError> {
    static KEY: OnceLock<Option<RsaPrivateKey>> = OnceLock::new();
    KEY.get_or_init(|| RsaPrivateKey::from_pkcs1_pem(AIRPORT_PEM).ok())
        .as_ref()
        .ok_or(RaopCryptoError::KeyUnavailable)
}

/// Unwrap the `a=rsaaeskey:` value from an `ANNOUNCE` into the AES-128 session key.
///
/// The wrapping is **RSA-OAEP with SHA-1** — not PKCS#1 v1.5, which is what the same
/// key is used with for [`sign_apple_challenge`]. Getting those two the wrong way round
/// produces a plausible-looking failure with no other symptom, so they are separate
/// functions rather than one with a mode flag.
///
/// # Errors
/// [`RaopCryptoError::Unwrap`] if the ciphertext does not decrypt (the usual cause is a
/// FairPlay-wrapped key), or [`RaopCryptoError::WrongKeyLength`] if what comes out is
/// not 16 bytes.
pub fn unwrap_aes_key(wrapped: &[u8]) -> Result<[u8; AES_KEY_LEN], RaopCryptoError> {
    let key = airport_key()?;
    let plain = key
        .decrypt(Oaep::new::<sha1::Sha1>(), wrapped)
        .map_err(|_| RaopCryptoError::Unwrap)?;
    <[u8; AES_KEY_LEN]>::try_from(plain.as_slice())
        .map_err(|_| RaopCryptoError::WrongKeyLength(plain.len()))
}

/// Sign an `Apple-Challenge`, producing the `Apple-Response` value.
///
/// A sender may send `Apple-Challenge` on `OPTIONS` and refuse to continue without a
/// valid `Apple-Response`; iTunes and macOS do, iOS is more forgiving. The signed
/// buffer is the challenge, then the address the sender reached us on, then our MAC,
/// zero-padded — so the answer is bound to *this* receiver on *this* interface and
/// cannot be replayed from a capture of another one.
///
/// The padding is **PKCS#1 v1.5 over the raw buffer**, with no digest and no DigestInfo
/// prefix: this is a raw private-key operation, not a signature over a hash.
///
/// # Errors
/// [`RaopCryptoError::ChallengeTooLong`] if the challenge exceeds 16 bytes, or
/// [`RaopCryptoError::Sign`] if the operation fails.
pub fn sign_apple_challenge(
    challenge: &[u8],
    local_addr: IpAddr,
    mac: [u8; 6],
) -> Result<Vec<u8>, RaopCryptoError> {
    if challenge.len() > MAX_CHALLENGE_LEN {
        return Err(RaopCryptoError::ChallengeTooLong(challenge.len()));
    }
    let key = airport_key()?;

    let mut buf = Vec::with_capacity(CHALLENGE_PAD_TO);
    buf.extend_from_slice(challenge);
    match local_addr {
        IpAddr::V4(v4) => buf.extend_from_slice(&v4.octets()),
        IpAddr::V6(v6) => buf.extend_from_slice(&v6.octets()),
    }
    buf.extend_from_slice(&mac);
    // Short buffers are zero-padded; a v6 address already takes it past the minimum.
    buf.resize(buf.len().max(CHALLENGE_PAD_TO), 0);

    key.sign(Pkcs1v15Sign::new_unprefixed(), &buf)
        .map_err(|_| RaopCryptoError::Sign)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rsa::traits::PublicKeyParts as _;
    use rsa::RsaPublicKey;

    #[test]
    fn the_embedded_key_is_the_2048_bit_airport_key() {
        let key = airport_key().unwrap();
        assert_eq!(key.size(), 256, "2048-bit modulus");
    }

    #[test]
    fn a_key_wrapped_to_the_public_half_round_trips() {
        // Exactly what a sender does: OAEP/SHA-1 to the public key that pairs with the
        // one we carry. If this passes, an `a=rsaaeskey:` from a real iPhone unwraps.
        let private = airport_key().unwrap();
        let public = RsaPublicKey::from(private);
        let session_key = *b"0123456789abcdef";
        let mut rng = rand_core_compat();
        let wrapped = public
            .encrypt(&mut rng, Oaep::new::<sha1::Sha1>(), &session_key)
            .unwrap();
        assert_eq!(unwrap_aes_key(&wrapped).unwrap(), session_key);
    }

    /// The RNG the `rsa` crate's encrypt path wants.
    fn rand_core_compat() -> impl rsa::rand_core::CryptoRngCore {
        rsa::rand_core::OsRng
    }

    #[test]
    fn a_fairplay_wrapped_key_fails_rather_than_producing_noise() {
        // et=3/et=5 wrap with FairPlay, not RSA. The failure has to be an error, not 16
        // arbitrary bytes that would decrypt the stream into static.
        let garbage = vec![0x11u8; 256];
        assert_eq!(unwrap_aes_key(&garbage), Err(RaopCryptoError::Unwrap));
    }

    #[test]
    fn a_challenge_is_signed_to_the_key_length() {
        let sig = sign_apple_challenge(
            b"0123456789abcdef",
            "10.0.0.9".parse().unwrap(),
            [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        )
        .unwrap();
        assert_eq!(sig.len(), 256);
    }

    #[test]
    fn the_signature_binds_the_address_and_mac_not_just_the_challenge() {
        // This is the point of the construction: a response captured from one receiver
        // must not verify for another. Same challenge, different box, different answer.
        let challenge = b"0123456789abcdef";
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let a = sign_apple_challenge(challenge, "10.0.0.9".parse().unwrap(), mac).unwrap();
        let b = sign_apple_challenge(challenge, "10.0.0.10".parse().unwrap(), mac).unwrap();
        assert_ne!(a, b, "the address must be part of what is signed");
        let c = sign_apple_challenge(challenge, "10.0.0.9".parse().unwrap(), [0; 6]).unwrap();
        assert_ne!(a, c, "the MAC must be part of what is signed");
    }

    #[test]
    fn signing_is_deterministic() {
        // PKCS#1 v1.5 has no randomness, so a sender retrying a challenge gets the same
        // answer — and this test would catch a switch to PSS, which would not verify.
        let args = (
            b"0123456789abcdef".as_slice(),
            "10.0.0.9".parse::<IpAddr>().unwrap(),
            [1, 2, 3, 4, 5, 6],
        );
        assert_eq!(
            sign_apple_challenge(args.0, args.1, args.2).unwrap(),
            sign_apple_challenge(args.0, args.1, args.2).unwrap()
        );
    }

    #[test]
    fn an_ipv6_address_is_signed_whole() {
        let sig = sign_apple_challenge(b"short", "fe80::1".parse().unwrap(), [1, 2, 3, 4, 5, 6]);
        assert!(sig.is_ok(), "a v6 address is 16 bytes and still fits");
    }

    #[test]
    fn an_over_long_challenge_is_refused() {
        assert_eq!(
            sign_apple_challenge(&[0u8; 17], "10.0.0.9".parse().unwrap(), [0; 6]),
            Err(RaopCryptoError::ChallengeTooLong(17))
        );
    }
}
