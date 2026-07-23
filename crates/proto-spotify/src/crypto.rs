//! The Spotify Connect zeroconf pairing crypto: Diffie-Hellman key agreement (Oakley
//! Group 1, generator 2) plus the librespot blob decryption (SHA1/HMAC-SHA1/AES-128-CTR).
//!
//! This is the genuinely reimplementable part of Spotify Connect. It's tested by
//! round-trip (our own [`encrypt_blob`] against [`decrypt_blob`]); validating the exact
//! byte framing against a *real* Spotify sender is an open question (OPEN-QUESTIONS).

use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use rand::RngCore;
use sha1::{Digest, Sha1};

use crate::error::SpotifyError;

type HmacSha1 = Hmac<Sha1>;
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// The 768-bit MODP prime (Oakley Group 1) Spotify uses for discovery DH.
const DH_PRIME: [u8; 96] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x3a, 0x36, 0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

/// A discovery DH keypair. The public key is published in `getInfo`; the private key
/// derives the shared secret with the sender's `clientKey`.
pub struct DhKeys {
    private: BigUint,
    public: BigUint,
}

impl DhKeys {
    /// Generate a fresh keypair using the given RNG source.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 95];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self::from_private_bytes(&bytes)
    }

    /// Build from explicit private-key bytes (deterministic; used in tests).
    #[must_use]
    pub fn from_private_bytes(private_bytes: &[u8]) -> Self {
        let prime = BigUint::from_bytes_be(&DH_PRIME);
        let generator = BigUint::from(2u32);
        let private = BigUint::from_bytes_be(private_bytes);
        let public = generator.modpow(&private, &prime);
        Self { private, public }
    }

    /// The DH public key, big-endian, left-padded to 96 bytes.
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        left_pad(&self.public.to_bytes_be(), 96)
    }

    /// Derive the shared secret from the sender's public key bytes.
    #[must_use]
    pub fn shared_secret(&self, remote_public: &[u8]) -> Vec<u8> {
        let prime = BigUint::from_bytes_be(&DH_PRIME);
        let remote = BigUint::from_bytes_be(remote_public);
        left_pad(&remote.modpow(&self.private, &prime).to_bytes_be(), 96)
    }
}

/// Derive the three librespot keys from a DH shared secret.
fn derive_keys(shared_key: &[u8]) -> Result<([u8; 16], Vec<u8>), SpotifyError> {
    let base_key = {
        let digest = Sha1::digest(shared_key);
        let mut k = [0u8; 16];
        k.copy_from_slice(&digest[..16]);
        k
    };
    let checksum_key = hmac_sha1(&base_key, b"checksum")?;
    let encryption_full = hmac_sha1(&base_key, b"encryption")?;
    let mut encryption_key = [0u8; 16];
    encryption_key.copy_from_slice(&encryption_full[..16]);
    Ok((encryption_key, checksum_key))
}

/// Decrypt a zeroconf `addUser` blob (base64-decoded bytes) using `shared_key`.
///
/// Layout: `iv(16) || ciphertext || hmac_sha1(20)`.
///
/// # Errors
/// [`SpotifyError::Crypto`] if the blob is too short or the checksum fails.
pub fn decrypt_blob(blob: &[u8], shared_key: &[u8]) -> Result<Vec<u8>, SpotifyError> {
    if blob.len() < 16 + 20 {
        return Err(SpotifyError::Crypto("blob too short"));
    }
    let (encryption_key, checksum_key) = derive_keys(shared_key)?;
    let iv = &blob[..16];
    let cksum = &blob[blob.len() - 20..];
    let ciphertext = &blob[16..blob.len() - 20];

    let expected = hmac_sha1(&checksum_key, ciphertext)?;
    if expected.as_slice() != cksum {
        return Err(SpotifyError::Crypto("blob checksum mismatch"));
    }

    let mut data = ciphertext.to_vec();
    let mut cipher = Aes128Ctr::new_from_slices(&encryption_key, iv)
        .map_err(|_| SpotifyError::Crypto("bad AES key/iv length"))?;
    cipher.apply_keystream(&mut data);
    Ok(data)
}

/// Encrypt a blob with the same scheme (the inverse of [`decrypt_blob`]) — used to
/// build round-trip test vectors until we capture a real one.
///
/// # Errors
/// [`SpotifyError::Crypto`] only if key derivation fails (not reachable for valid input).
pub fn encrypt_blob(
    plaintext: &[u8],
    shared_key: &[u8],
    iv: &[u8; 16],
) -> Result<Vec<u8>, SpotifyError> {
    let (encryption_key, checksum_key) = derive_keys(shared_key)?;
    let mut data = plaintext.to_vec();
    let mut cipher = Aes128Ctr::new(encryption_key[..].into(), iv[..].into());
    cipher.apply_keystream(&mut data);
    let cksum = hmac_sha1(&checksum_key, &data)?;
    let mut out = Vec::with_capacity(16 + data.len() + 20);
    out.extend_from_slice(iv);
    out.extend_from_slice(&data);
    out.extend_from_slice(&cksum);
    Ok(out)
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> Result<Vec<u8>, SpotifyError> {
    let mut mac = <HmacSha1 as Mac>::new_from_slice(key)
        .map_err(|_| SpotifyError::Crypto("invalid HMAC key length"))?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn left_pad(bytes: &[u8], len: usize) -> Vec<u8> {
    if bytes.len() >= len {
        return bytes[bytes.len() - len..].to_vec();
    }
    let mut out = vec![0u8; len - bytes.len()];
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn dh_parties_agree_on_secret() {
        let a = DhKeys::from_private_bytes(&[3u8; 95]);
        let b = DhKeys::from_private_bytes(&[7u8; 95]);
        let secret_ab = a.shared_secret(&b.public_key());
        let secret_ba = b.shared_secret(&a.public_key());
        assert_eq!(secret_ab, secret_ba);
        assert_eq!(secret_ab.len(), 96);
    }

    #[test]
    fn public_key_is_96_bytes() {
        let k = DhKeys::from_private_bytes(&[1u8; 95]);
        assert_eq!(k.public_key().len(), 96);
    }

    #[test]
    fn blob_roundtrips() {
        let shared = DhKeys::from_private_bytes(&[9u8; 95]).public_key();
        let iv = [0x11u8; 16];
        let plaintext = b"credentials-blob-contents-here";
        let blob = encrypt_blob(plaintext, &shared, &iv).unwrap();
        let back = decrypt_blob(&blob, &shared).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn tampered_blob_fails_checksum() {
        let shared = DhKeys::from_private_bytes(&[9u8; 95]).public_key();
        let mut blob = encrypt_blob(b"data", &shared, &[0u8; 16]).unwrap();
        blob[20] ^= 0xff; // corrupt ciphertext
        assert!(decrypt_blob(&blob, &shared).is_err());
    }

    #[test]
    fn short_blob_rejected() {
        assert!(decrypt_blob(&[0u8; 10], &[0u8; 96]).is_err());
    }
}
