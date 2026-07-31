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

/// Build the *inner* credentials blob — the one a real Spotify sender wraps and posts to
/// `addUser`, and the one librespot's `Credentials::with_blob` unwraps.
///
/// This is the encoder for a format we otherwise only ever decode, and it exists to make
/// the pairing path testable end to end: with it, a scripted sender can pair with the
/// receiver, and our understanding of the layout can be checked against librespot's real
/// decoder rather than against our own decoder (#48). That is a genuine
/// cross-implementation check — librespot's side was derived from real senders.
///
/// Layout, mirroring the reader exactly: a discarded byte, a discarded length-prefixed
/// field, a discarded byte, the varint auth type, a discarded byte, then the credential
/// bytes. The result is zero-padded to a whole AES block, because the decoder walks
/// `chunks_exact(16)` and would silently leave a trailing partial block encrypted.
///
/// # Errors
/// [`SpotifyError::Crypto`] if key derivation fails.
pub fn encode_credentials_blob(
    username: &str,
    device_id: &str,
    auth_type: u32,
    auth_data: &[u8],
) -> Result<Vec<u8>, SpotifyError> {
    use aes::cipher::{BlockEncrypt, KeyInit};

    let mut plain = Vec::new();
    plain.push(b'I');
    write_bytes(&mut plain, username.as_bytes());
    plain.push(b'P');
    write_int(&mut plain, auth_type);
    plain.push(b'Q');
    write_bytes(&mut plain, auth_data);
    // Pad to a whole block. Content beyond what the reader consumes is ignored, so zeros
    // are safe; leaving a partial block is not.
    while plain.len() % 16 != 0 {
        plain.push(0);
    }

    // The chaining step, run backwards. The decoder walks `i` upward applying
    // `data[l-i-1] ^= data[l-i-0x11]`, and each source byte is itself rewritten later in
    // that same loop — so inverting it means replaying the identical operation with `i`
    // descending, not XOR-ing in the same order.
    let l = plain.len();
    for i in (0..l.saturating_sub(0x10)).rev() {
        plain[l - i - 1] ^= plain[l - i - 0x11];
    }

    let key = blob_key(username, device_id)?;
    let cipher =
        aes::Aes192::new_from_slice(&key).map_err(|_| SpotifyError::Crypto("bad AES-192 key"))?;
    for chunk in plain.chunks_exact_mut(16) {
        cipher.encrypt_block(chunk.into());
    }
    Ok(plain)
}

/// Derive the AES-192 key that wraps the inner blob: PBKDF2-SHA1 over `SHA1(device_id)`
/// salted with the username, then hashed again, with the length appended.
fn blob_key(username: &str, device_id: &str) -> Result<[u8; 24], SpotifyError> {
    let secret = Sha1::digest(device_id.as_bytes());
    let mut key = [0u8; 24];
    pbkdf2::pbkdf2_hmac::<Sha1>(&secret, username.as_bytes(), 0x100, &mut key[0..20]);
    let hash = Sha1::digest(&key[..20]);
    key[..20].copy_from_slice(&hash);
    key[20..].copy_from_slice(&20u32.to_be_bytes());
    Ok(key)
}

/// The reader's variable-length integer: seven bits per byte, low byte first, high bit
/// meaning "one more byte follows". Only ever two bytes wide in practice.
fn write_int(out: &mut Vec<u8>, value: u32) {
    #[allow(clippy::cast_possible_truncation)]
    if value < 0x80 {
        out.push(value as u8);
    } else {
        out.push((value & 0x7f) as u8 | 0x80);
        out.push((value >> 7) as u8);
    }
}

/// A length-prefixed byte string, where the length is a [`write_int`].
fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    #[allow(clippy::cast_possible_truncation)]
    write_int(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
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
    use base64::Engine as _;

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

    /// The check #48 actually wants, as far as it can be had without a phone.
    ///
    /// Every other test in this file round-trips our own code against itself, which
    /// cannot catch a misunderstanding of the format — only an inconsistency. This one
    /// hands our encoder's output to *librespot's* decoder, which was derived from real
    /// senders. It still is not a captured `addUser`, but it is a second opinion.
    #[test]
    fn our_inner_blob_is_one_librespot_can_read() {
        use librespot_core::authentication::Credentials;

        const DEVICE_ID: &str = "0f8c2e10castaway0001000000000001";
        // AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS — what a real pairing carries.
        const STORED: u32 = 1;
        let auth_data = b"reusable-credential-bytes-from-an-APWelcome".to_vec();

        let blob = encode_credentials_blob("alice", DEVICE_ID, STORED, &auth_data).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);

        let creds = Credentials::with_blob("alice", &b64, DEVICE_ID)
            .expect("librespot should decode a blob we encoded");
        assert_eq!(creds.username.as_deref(), Some("alice"));
        assert_eq!(creds.auth_data, auth_data);
    }

    #[test]
    fn the_blob_key_is_bound_to_the_device_and_the_user() {
        // Both are inputs to the derivation, so a receiver that advertised one device id
        // and decrypted with another gets an unreadable blob — the failure mode that
        // looks exactly like "pairing expired" from the phone.
        use librespot_core::authentication::Credentials;

        let blob = encode_credentials_blob("alice", "device-one", 1, b"creds").unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
        assert!(Credentials::with_blob("alice", &b64, "device-two").is_err());
        assert!(Credentials::with_blob("bob", &b64, "device-one").is_err());
        assert!(Credentials::with_blob("alice", &b64, "device-one").is_ok());
    }

    #[test]
    fn a_full_pairing_survives_both_layers() {
        // The whole sender path: build the inner blob, wrap it the way a phone wraps it
        // (DH + AES-CTR + HMAC), then unwrap with the receiver's key and decode.
        use librespot_core::authentication::Credentials;

        const DEVICE_ID: &str = "deadbeefdeadbeefdeadbeefdeadbeef";
        let receiver = DhKeys::from_private_bytes(&[4u8; 95]);
        let phone = DhKeys::from_private_bytes(&[6u8; 95]);
        let shared = phone.shared_secret(&receiver.public_key());

        let inner = encode_credentials_blob("alice", DEVICE_ID, 1, b"creds").unwrap();
        let outer = encrypt_blob(&inner, &shared, &[7u8; 16]).unwrap();

        let recovered = decrypt_blob(&outer, &receiver.shared_secret(&phone.public_key())).unwrap();
        assert_eq!(recovered, inner);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&recovered);
        let creds = Credentials::with_blob("alice", &b64, DEVICE_ID).unwrap();
        assert_eq!(creds.auth_data, b"creds");
    }
}
