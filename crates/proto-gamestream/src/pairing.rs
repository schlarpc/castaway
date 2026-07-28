//! The gen-7 GameStream pairing handshake — the pure core.
//!
//! Pairing is four HTTP round trips that convince the host to trust our certificate,
//! keyed by a 4-digit PIN a person types into the host (Sunshine's web UI). The crypto
//! is modest but every byte placement matters, so this module is a typestate machine:
//! each phase is its own type, a response can only be fed to the phase that expects it,
//! and the values a later phase needs are carried, not re-derived. All randomness is
//! injected through [`PairingSeed`], which is what makes the golden-vector tests below
//! possible (ground rule 6). No I/O here: bytes in, bytes out (ground rule 3).
//!
//! The shape (docs/gamestream-protocol-notes.md §3; Sunshine `nvhttp.cpp`,
//! moonlight-qt `nvpairingmanager.cpp`):
//!
//! 1. `getservercert`: salt + our cert (hex of the PEM *text*) → host's cert. The AES
//!    key for everything after is `SHA-256(salt ‖ PIN-as-ASCII)[..16]`, AES-128-ECB,
//!    no padding, no IV.
//! 2. `clientchallenge`: our random 16 bytes, encrypted → the host answers with
//!    `ECB(SHA-256(challenge ‖ serverCertSig ‖ serverSecret) ‖ serverChallenge)`.
//! 3. `serverchallengeresp`: `ECB(SHA-256(serverChallenge ‖ clientCertSig ‖
//!    clientSecret))` → the host hands over `serverSecret ‖ RSA-sign(serverSecret)`.
//!    *Now* we can check both the host's signature (MITM) and the hash it sent in
//!    phase 2 (wrong PIN) — the checks are deliberately deferred to here.
//! 4. `clientpairingsecret`: `clientSecret ‖ RSA-sign(clientSecret)` → `<paired>1`.
//!
//! The "cert signature" hashed on both sides is the X.509 `signatureValue` BIT STRING
//! — the raw 256 signature bytes at the end of the DER, not a digest of the cert.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

use crate::error::GameStreamError;
use crate::identity::ClientIdentity;

/// The randomness one pairing attempt consumes. Injected so tests are deterministic;
/// the adapter fills it from the OS.
#[derive(Clone, Copy)]
pub struct PairingSeed {
    /// Salts the PIN into the AES key; sent to the host in the clear.
    pub salt: [u8; 16],
    /// Our challenge — proves the host derived the same AES key (i.e. saw the PIN).
    pub challenge: [u8; 16],
    /// Our secret — what we ultimately sign to prove we hold the cert's key.
    pub secret: [u8; 16],
}

/// The `/pair` query values a phase asks the transport to send. Values only — the
/// `nvhttp` module owns parameter names and ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRequest {
    /// The one phase-specific `key=value` pair, pre-hex-encoded (lowercase).
    pub param: (&'static str, String),
    /// `phrase=` value when the phase carries one (`getservercert`/`pairchallenge`).
    pub phrase: Option<&'static str>,
    /// Extra parameters the phase needs (`salt` on phase 1).
    pub extra: Vec<(&'static str, String)>,
}

/// What a completed pairing hands back: the host certificate we verified, which
/// becomes the pinned TLS anchor for every future HTTPS request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedServer {
    /// The host certificate PEM, exactly as `plaincert` delivered it.
    pub server_cert_pem: String,
    /// The same certificate as DER, for rustls pinning.
    pub server_cert_der: Vec<u8>,
}

/// Phase 1 sent, waiting for `plaincert`.
pub struct AwaitingServerCert {
    aes_key: [u8; 16],
    challenge: [u8; 16],
    secret: [u8; 16],
    client_cert_sig: Vec<u8>,
    client_key: RsaPrivateKey,
}

/// Phase 2 sent, waiting for `challengeresponse`.
pub struct AwaitingChallengeResponse {
    aes_key: [u8; 16],
    challenge: [u8; 16],
    secret: [u8; 16],
    client_cert_sig: Vec<u8>,
    client_key: RsaPrivateKey,
    server: PairedServer,
}

/// Phase 3 sent, waiting for `pairingsecret`.
pub struct AwaitingPairingSecret {
    challenge: [u8; 16],
    secret: [u8; 16],
    client_key: RsaPrivateKey,
    server: PairedServer,
    /// The host's phase-2 hash, checked against the PIN-derived expectation once the
    /// server secret arrives.
    server_response_hash: Vec<u8>,
}

/// Phase 4 sent, waiting for the final `<paired>` flag.
#[derive(Debug)]
pub struct AwaitingPairedFlag {
    server: PairedServer,
}

/// Begin a pairing attempt. Returns the phase-1 request (`phrase=getservercert`) and
/// the state that understands its response.
///
/// The PIN is hashed as the ASCII text of its digits — leading zeros matter.
#[must_use]
pub fn start(
    identity: &ClientIdentity,
    pin: &str,
    seed: PairingSeed,
) -> (AwaitingServerCert, PhaseRequest) {
    let mut hasher = Sha256::new();
    hasher.update(seed.salt);
    hasher.update(pin.as_bytes());
    let digest = hasher.finalize();
    let mut aes_key = [0u8; 16];
    aes_key.copy_from_slice(&digest[..16]);

    let request = PhaseRequest {
        param: ("clientcert", hex_encode(identity.cert_pem().as_bytes())),
        phrase: Some("getservercert"),
        extra: vec![("salt", hex_encode(&seed.salt))],
    };
    let state = AwaitingServerCert {
        aes_key,
        challenge: seed.challenge,
        secret: seed.secret,
        // Our own cert always parses; a failure here is a bug, surfaced as an error
        // rather than a panic all the same.
        client_cert_sig: cert_signature_bits(identity.cert_der()).unwrap_or_default(),
        client_key: identity.key().clone(),
    };
    (state, request)
}

impl AwaitingServerCert {
    /// Feed the phase-1 `plaincert` (hex of the host cert PEM). Produces the phase-2
    /// `clientchallenge` request.
    ///
    /// # Errors
    /// [`GameStreamError::Pairing`] when the cert is missing (another pairing is in
    /// flight on the host) or unparseable.
    pub fn on_server_cert(
        self,
        plaincert_hex: &str,
    ) -> Result<(AwaitingChallengeResponse, PhaseRequest), GameStreamError> {
        if plaincert_hex.is_empty() {
            // Sunshine answers an overlapping pairing attempt with an empty plaincert.
            return Err(GameStreamError::Pairing(
                "host sent no certificate — is another pairing already in progress?".into(),
            ));
        }
        let pem_bytes = hex_decode(plaincert_hex)
            .ok_or_else(|| GameStreamError::Pairing("plaincert is not valid hex".into()))?;
        let server_cert_pem = String::from_utf8(pem_bytes)
            .map_err(|_| GameStreamError::Pairing("plaincert is not UTF-8 PEM".into()))?;
        let server_cert_der = pem_to_der(&server_cert_pem)
            .ok_or_else(|| GameStreamError::Pairing("plaincert PEM did not parse".into()))?;
        // Parsed now so a garbage cert fails pairing rather than phase 3.
        cert_signature_bits(&server_cert_der)
            .ok_or_else(|| GameStreamError::Pairing("host certificate DER is malformed".into()))?;

        let request = PhaseRequest {
            param: (
                "clientchallenge",
                hex_encode(&ecb_encrypt(&self.aes_key, &self.challenge)?),
            ),
            phrase: None,
            extra: Vec::new(),
        };
        Ok((
            AwaitingChallengeResponse {
                aes_key: self.aes_key,
                challenge: self.challenge,
                secret: self.secret,
                client_cert_sig: self.client_cert_sig,
                client_key: self.client_key,
                server: PairedServer {
                    server_cert_pem,
                    server_cert_der,
                },
            },
            request,
        ))
    }
}

impl AwaitingChallengeResponse {
    /// Feed the phase-2 `challengeresponse`. Produces the phase-3
    /// `serverchallengeresp` request.
    ///
    /// # Errors
    /// [`GameStreamError::Pairing`] on malformed hex or a truncated payload.
    pub fn on_challenge_response(
        self,
        challengeresponse_hex: &str,
    ) -> Result<(AwaitingPairingSecret, PhaseRequest), GameStreamError> {
        let ciphertext = hex_decode(challengeresponse_hex)
            .ok_or_else(|| GameStreamError::Pairing("challengeresponse is not hex".into()))?;
        let plaintext = ecb_decrypt(&self.aes_key, &ciphertext)?;
        // SHA-256 hash (32) ‖ server challenge (16).
        if plaintext.len() < 48 {
            return Err(GameStreamError::Pairing(format!(
                "challengeresponse decrypted to {} bytes, need 48",
                plaintext.len()
            )));
        }
        let server_response_hash = plaintext[..32].to_vec();
        let mut server_challenge = [0u8; 16];
        server_challenge.copy_from_slice(&plaintext[32..48]);

        // SHA-256( serverChallenge ‖ our cert signature ‖ our secret ), encrypted.
        let mut hasher = Sha256::new();
        hasher.update(server_challenge);
        hasher.update(&self.client_cert_sig);
        hasher.update(self.secret);
        let response_hash = hasher.finalize();
        let request = PhaseRequest {
            param: (
                "serverchallengeresp",
                hex_encode(&ecb_encrypt(&self.aes_key, &response_hash)?),
            ),
            phrase: None,
            extra: Vec::new(),
        };

        Ok((
            AwaitingPairingSecret {
                challenge: self.challenge,
                secret: self.secret,
                client_key: self.client_key,
                server: self.server,
                server_response_hash,
            },
            request,
        ))
    }
}

impl AwaitingPairingSecret {
    /// Feed the phase-3 `pairingsecret`. This is where trust is decided: the host's
    /// signature over its secret is verified against the phase-1 certificate (a
    /// failure is a MITM or a host that lost its key), and the hash it committed to in
    /// phase 2 is recomputed (a mismatch is the PIN). Produces the phase-4
    /// `clientpairingsecret` request.
    ///
    /// # Errors
    /// [`GameStreamError::WrongPin`] for the hash mismatch;
    /// [`GameStreamError::Pairing`] for everything trust-breaking.
    pub fn on_pairing_secret(
        self,
        pairingsecret_hex: &str,
    ) -> Result<(AwaitingPairedFlag, PhaseRequest), GameStreamError> {
        let pairing_secret = hex_decode(pairingsecret_hex)
            .ok_or_else(|| GameStreamError::Pairing("pairingsecret is not hex".into()))?;
        if pairing_secret.len() <= 16 {
            return Err(GameStreamError::Pairing(
                "pairingsecret too short to carry a signature".into(),
            ));
        }
        let (server_secret, server_signature) = pairing_secret.split_at(16);

        // MITM check: the secret must be signed by the certificate from phase 1.
        let server_public = cert_rsa_public_key(&self.server.server_cert_der)
            .ok_or_else(|| GameStreamError::Pairing("host certificate has no RSA key".into()))?;
        let signature = Signature::try_from(server_signature).map_err(|_| {
            GameStreamError::Pairing("host signature is not an RSA signature".into())
        })?;
        VerifyingKey::<Sha256>::new(server_public)
            .verify(server_secret, &signature)
            .map_err(|_| {
                GameStreamError::Pairing(
                    "host signature over its secret failed to verify — \
                     possible man-in-the-middle"
                        .into(),
                )
            })?;

        // PIN check: what the host hashed in phase 2 must match what we compute now
        // that we hold its secret. Sunshine hashed (our decrypted challenge ‖ its cert
        // signature ‖ its secret); if it derived a different AES key from a different
        // PIN, our challenge decrypted to garbage on its side and this differs.
        let server_cert_sig = cert_signature_bits(&self.server.server_cert_der)
            .ok_or_else(|| GameStreamError::Pairing("host certificate DER is malformed".into()))?;
        let mut hasher = Sha256::new();
        hasher.update(self.challenge);
        hasher.update(&server_cert_sig);
        hasher.update(server_secret);
        let expected: [u8; 32] = hasher.finalize().into();
        if expected.as_slice() != self.server_response_hash.as_slice() {
            return Err(GameStreamError::WrongPin);
        }

        // Our proof: secret ‖ RSASSA-PKCS1-v1_5-SHA256(secret).
        let our_signature = SigningKey::<Sha256>::new(self.client_key).sign(&self.secret);
        let mut client_pairing_secret = Vec::with_capacity(16 + 256);
        client_pairing_secret.extend_from_slice(&self.secret);
        client_pairing_secret.extend_from_slice(&our_signature.to_vec());

        let request = PhaseRequest {
            param: ("clientpairingsecret", hex_encode(&client_pairing_secret)),
            phrase: None,
            extra: Vec::new(),
        };
        Ok((
            AwaitingPairedFlag {
                server: self.server,
            },
            request,
        ))
    }
}

impl AwaitingPairedFlag {
    /// Feed the phase-4 `<paired>` flag. Sunshine reports its rejection with
    /// `status_code=200` and `<paired>0`, so this flag — not the HTTP status — is the
    /// verdict. On success the caller still owes the host the HTTPS
    /// `phrase=pairchallenge` round trip (that is what proves mutual TLS now works),
    /// but no state is needed for it.
    ///
    /// # Errors
    /// [`GameStreamError::Pairing`] when the host said no.
    pub fn finish(self, paired: bool) -> Result<PairedServer, GameStreamError> {
        if paired {
            Ok(self.server)
        } else {
            Err(GameStreamError::Pairing(
                "host rejected the pairing secret (paired=0)".into(),
            ))
        }
    }
}

/// The phase-5 request (`phrase=pairchallenge`), sent over HTTPS with the new cert.
#[must_use]
pub fn pair_challenge_request() -> PhaseRequest {
    PhaseRequest {
        param: ("phrase", "pairchallenge".into()),
        phrase: None,
        extra: Vec::new(),
    }
}

// --- primitives -------------------------------------------------------------------

/// AES-128-ECB with padding disabled — the input must already be block-aligned.
fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, GameStreamError> {
    ecb(key, data, true)
}

fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, GameStreamError> {
    ecb(key, data, false)
}

fn ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Result<Vec<u8>, GameStreamError> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(GameStreamError::Pairing(format!(
            "pairing payload of {} bytes is not AES-block aligned",
            data.len()
        )));
    }
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        if encrypt {
            cipher.encrypt_block(block.into());
        } else {
            cipher.decrypt_block(block.into());
        }
    }
    Ok(out)
}

/// Lowercase hex, natural byte order — the only form we ever emit (Sunshine's parser
/// silently mis-decodes some non-hex letters rather than rejecting them, so strictness
/// is on us).
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Case-insensitive strict hex decode; `None` on odd length or a non-hex digit.
#[must_use]
pub fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    hex.chunks_exact(2)
        .map(|pair| Some(nib(pair[0])? << 4 | nib(pair[1])?))
        .collect()
}

/// Encode DER as a PEM block. Written here rather than taken from rcgen's `pem`
/// feature because this text is *hashed and shipped verbatim* during pairing — the
/// 64-column wrapping and the trailing newline are wire-visible, so they belong
/// somewhere a test can pin them.
#[must_use]
pub fn der_to_pem(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Decode the first PEM block's base64 body to DER. Label-agnostic on purpose: it
/// serves certificates here and keys in `identity`.
#[must_use]
pub fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    if body.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

// A certificate is `SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }`.
// The two extractors below walk exactly as far as they need and no further — this is
// not an X.509 parser, it is the two byte-slices pairing hashes and verifies with.

/// The contents of the `signatureValue` BIT STRING (unused-bits byte stripped).
#[must_use]
pub fn cert_signature_bits(cert_der: &[u8]) -> Option<Vec<u8>> {
    let mut outer = DerCursor::new(cert_der);
    let (tag, cert_body) = outer.tlv()?;
    if tag != 0x30 {
        return None;
    }
    let mut cert = DerCursor::new(cert_body);
    cert.tlv()?; // tbsCertificate
    cert.tlv()?; // signatureAlgorithm
    let (tag, bits) = cert.tlv()?;
    if tag != 0x03 || bits.is_empty() || bits[0] != 0 {
        return None;
    }
    Some(bits[1..].to_vec())
}

/// The RSA public key out of the certificate's SubjectPublicKeyInfo.
fn cert_rsa_public_key(cert_der: &[u8]) -> Option<RsaPublicKey> {
    let mut outer = DerCursor::new(cert_der);
    let (0x30, cert_body) = outer.tlv()? else {
        return None;
    };
    let mut cert = DerCursor::new(cert_body);
    let (0x30, tbs) = cert.tlv()? else {
        return None;
    };
    let mut tbs = DerCursor::new(tbs);
    // version [0] EXPLICIT is optional; serial, sigalg, issuer, validity, subject.
    let (first_tag, _) = tbs.peek()?;
    if first_tag == 0xA0 {
        tbs.tlv()?;
    }
    for _ in 0..5 {
        tbs.tlv()?;
    }
    let (0x30, spki) = tbs.tlv()? else {
        return None;
    };
    let mut spki = DerCursor::new(spki);
    spki.tlv()?; // AlgorithmIdentifier — rsaEncryption assumed; parse decides below.
    let (0x03, key_bits) = spki.tlv()? else {
        return None;
    };
    if key_bits.is_empty() || key_bits[0] != 0 {
        return None;
    }
    RsaPublicKey::from_pkcs1_der(&key_bits[1..]).ok()
}

/// The little DER walk the two extractors share. Definite lengths only, which is all
/// DER allows anyway.
struct DerCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> DerCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<(u8, &'a [u8])> {
        let mut copy = DerCursor {
            bytes: self.bytes,
            pos: self.pos,
        };
        copy.tlv()
    }

    /// Next tag + content, advancing past it.
    fn tlv(&mut self) -> Option<(u8, &'a [u8])> {
        let tag = *self.bytes.get(self.pos)?;
        let mut idx = self.pos + 1;
        let first = *self.bytes.get(idx)?;
        idx += 1;
        let len = if first < 0x80 {
            usize::from(first)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > 4 {
                return None;
            }
            let mut len = 0usize;
            for _ in 0..n {
                len = len << 8 | usize::from(*self.bytes.get(idx)?);
                idx += 1;
            }
            len
        };
        let content = self.bytes.get(idx..idx + len)?;
        self.pos = idx + len;
        Some((tag, content))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    /// The host credentials Sunshine's own pairing unit test uses
    /// (`tests/unit/test_http_pairing.cpp`, GamesOnWhales/localhost). Its checked-in
    /// ciphertexts are golden vectors for every primitive here.
    const SUNSHINE_TEST_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIC6zCCAdOgAwIBAgIBATANBgkqhkiG9w0BAQsFADA5MQswCQYDVQQGEwJJVDEW\n\
MBQGA1UECgwNR2FtZXNPbldoYWxlczESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTIy\n\
MDQwOTA5MTYwNVoXDTQyMDQwNDA5MTYwNVowOTELMAkGA1UEBhMCSVQxFjAUBgNV\n\
BAoMDUdhbWVzT25XaGFsZXMxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZI\n\
hvcNAQEBBQADggEPADCCAQoCggEBAMt482VY3ToUuUy6NbMhfxQgI7tJZ8fkNeVp\n\
9WOnHCL9YKR07oXGLGpE0a7vXAy8lcVsOU1Hx+pfbGj56rXsne4Uqf6p2OY/cvfx\n\
uSrGGgn+cKteR4bIJND4Nq6DrdlhIl5bYyZ/4sBHn+L99Zh9elKVtx/lclA8Ra8Q\n\
2kupa7405TnR0lcgRVilRdHHb7HhlvCQfu1Umb3gv4I5TKIkpA/JaBTZoWzIkbAc\n\
V9499JSl9gepsdlX8guljn1UlqKsHAT31vH+YG8wjtqEGYlNIO4N98lw8OEUXmRl\n\
rRSRA+s++FdxBpJG2Lu/RWicRCPylNKcZiv2S1YqT3bDEPKf1LcCAwEAATANBgkq\n\
hkiG9w0BAQsFAAOCAQEAqPBqzvDjl89pZMll3Ge8RS7HeDuzgocrhOcT2jnk4ag7\n\
/TROZuISjDp6+SnL3gPEt7E2OcFAczTg3l/wbT5PFb6vM96saLm4EP0zmLfK1FnM\n\
JDRahKutP9rx6RO5OHqsUB+b4jA4W0L9UnXUoLKbjig501AUix0p52FBxu+HJ90r\n\
HlLs3Vo6nj4Z/PZXrzaz8dtQ/KJMpd/g/9xlo6BKAnRk5SI8KLhO4hW6zG0QA56j\n\
X4wnh1bwdiidqpcgyuKossLOPxbS786WmsesaAWPnpoY6M8aija+ALwNNuWWmyMg\n\
9SVDV76xJzM36Uq7Kg3QJYTlY04WmPIdJHkCtXWf9g==\n\
-----END CERTIFICATE-----\n";

    /// `salt` and `pin` from the same test.
    const SALT: [u8; 16] = hex!("ff5dc6eda99339a8a0793e216c4257c4");
    const PIN: &str = "5338";

    fn sunshine_aes_key() -> [u8; 16] {
        let mut hasher = Sha256::new();
        hasher.update(SALT);
        hasher.update(PIN.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        key
    }

    #[test]
    fn kdf_and_ecb_match_sunshines_client_challenge_vector() {
        // Sunshine's test: AES("CLIENT CHALLENGE") == 741C… under the salt+PIN key.
        let ct = ecb_encrypt(&sunshine_aes_key(), b"CLIENT CHALLENGE").unwrap();
        assert_eq!(
            ct,
            hex!("741CD3D6890C16DA39D53BCA0893AAF0"),
            "AES key derivation or ECB disagrees with Sunshine's checked-in vector"
        );
        // And the inverse direction.
        let pt = ecb_decrypt(&sunshine_aes_key(), &ct).unwrap();
        assert_eq!(&pt, b"CLIENT CHALLENGE");
    }

    #[test]
    fn cert_signature_extraction_matches_sunshines_hash_vector() {
        // This is the phase-4 hash Sunshine recomputes to decide whether we paired:
        // SHA-256( serverChallenge ‖ signature(clientCert) ‖ clientSecret ).
        //
        // The vector comes from Sunshine's own pairing test, where the session's
        // server challenge is overridden to eight 0xAA bytes and the client secret is
        // 0001…0EFF. Note that eight bytes is *not* the sixteen a real challenge has:
        // nothing in the hash is length-prefixed, so Sunshine hashes whatever it
        // holds, and its test exploits that. The comment beside the vector in
        // test_http_pairing.cpp claims the third field is the ASCII `"SECRET  "`; it
        // is not, and the bytes here are what settles it — the third field is the
        // 16-byte client secret from the same test's `client_pairing_secret`.
        //
        // Only passes if we extract exactly the signatureValue BIT STRING contents.
        let der = pem_to_der(SUNSHINE_TEST_CERT).unwrap();
        let sig = cert_signature_bits(&der).unwrap();
        assert_eq!(sig.len(), 256, "RSA-2048 cert signature must be 256 bytes");
        let mut hasher = Sha256::new();
        hasher.update([0xAA; 8]);
        hasher.update(&sig);
        hasher.update(hex!("000102030405060708090A0B0C0D0EFF"));
        let digest: [u8; 32] = hasher.finalize().into();
        assert_eq!(
            digest,
            hex!("6493DAE49C913E1AEAF37C1072F71D664B72B2C4DA1FFB4720BECE0D929E008A"),
            "certificate signatureValue extraction disagrees with Sunshine"
        );
        // And the ciphertext vector, closing the loop over the ECB layer: this is
        // byte-for-byte the `serverchallengeresp` Sunshine's test feeds its phase 3.
        assert_eq!(
            ecb_encrypt(&sunshine_aes_key(), &digest).unwrap(),
            hex!(
                "920BABAE9F7599AA1CA8EC87FB3454C9"
                "1872A7D8D5127DDC176C2FDAE635CF7A"
            ),
            "our AES key or ECB layer would produce a serverchallengeresp Sunshine rejects"
        );
    }

    #[test]
    fn sunshines_client_pairing_secret_signature_verifies_against_its_cert() {
        // The clientpairingsecret vector: secret 000102…0EFF signed by the test key.
        // Verifying it against the cert's public key proves our SPKI extraction and
        // PKCS1v15 verify agree with what Sunshine accepts.
        let der = pem_to_der(SUNSHINE_TEST_CERT).unwrap();
        let public = cert_rsa_public_key(&der).unwrap();
        let secret = hex!("000102030405060708090A0B0C0D0EFF");
        let signature = hex!(
            "9BB74D8DE2FF006C3F47FC45EFDAA97D433783AFAB3ACD85CA7ED2330BB2A7BD"
            "18A5B044AF8CAC177116FAE8A6E8E44653A8944A0F8EA138B2E013756D847D2C"
            "4FC52F736E2E7E9B4154712B18F8307B2A161E010F0587744163E42ECA9EA548"
            "FC435756EDCF1FEB94037631ABB72B29DDAC0EA5E61F2DBFCC3B20AA021473CC"
            "85AC98D88052CA6618ED1701EFBF142C18D5E779A3155B84DF65057D4823EC19"
            "4E6DF14006793E8D7A3DCCE20A911636C4E01ECA8B54B9DE9F256F15DE9A980E"
            "A024B30D77579140D45EC220C738164BDEEEBF7364AE94A5FF9B784B40F2E640"
            "CE8603017DEEAC7B2AD77B807C643B7B349C110FE15F94C7B3D37FF15FDFBE26"
        );
        let vk = VerifyingKey::<Sha256>::new(public);
        vk.verify(&secret, &Signature::try_from(&signature[..]).unwrap())
            .expect("Sunshine's golden clientpairingsecret signature must verify");
        // And a flipped bit must not.
        let mut bad = secret;
        bad[0] ^= 1;
        assert!(vk
            .verify(&bad, &Signature::try_from(&signature[..]).unwrap())
            .is_err());
    }

    /// Drive the whole client state machine against an in-test host implementing
    /// Sunshine's side of the handshake (from nvhttp.cpp), sharing the PIN. Every
    /// phase's byte layout is exercised in both directions.
    #[test]
    fn full_handshake_against_a_faithful_host() {
        let identity = ClientIdentity::generate().unwrap();
        let seed = PairingSeed {
            salt: SALT,
            challenge: *b"CLIENT CHALLENGE",
            secret: hex!("000102030405060708090A0B0C0D0EFF"),
        };

        // Host state (Sunshine uses its own RSA cert; the test cert has no key we
        // hold, so the host here gets a generated identity too).
        let host = ClientIdentity::generate().unwrap();
        let host_sig = cert_signature_bits(host.cert_der()).unwrap();
        let key = sunshine_aes_key();
        let server_secret = hex!("53554e5348494e455f5345435245542e"); // "SUNSHINE_SECRET."
        let server_challenge = *b"SERVER CHALLENGE";

        let (state, p1) = start(&identity, PIN, seed);
        assert_eq!(p1.phrase, Some("getservercert"));
        assert_eq!(p1.extra, vec![("salt", hex_encode(&SALT))]);
        // Host: answers with its cert PEM, hex-encoded (uppercase, like Sunshine).
        let plaincert = hex_encode(host.cert_pem().as_bytes()).to_uppercase();

        let (state, p2) = state.on_server_cert(&plaincert).unwrap();
        // Host: decrypts our challenge, hashes, answers hash ‖ its challenge.
        let client_challenge = ecb_decrypt(&key, &hex_decode(&p2.param.1).unwrap()).unwrap();
        assert_eq!(client_challenge, seed.challenge);
        let mut hasher = Sha256::new();
        hasher.update(&client_challenge);
        hasher.update(&host_sig);
        hasher.update(server_secret);
        let mut plaintext = hasher.finalize().to_vec();
        plaintext.extend_from_slice(&server_challenge);
        let challengeresponse = hex_encode(&ecb_encrypt(&key, &plaintext).unwrap());

        let (state, p3) = state.on_challenge_response(&challengeresponse).unwrap();
        // Host: stores clienthash (checked in its phase 4), sends secret ‖ signature.
        let client_hash = ecb_decrypt(&key, &hex_decode(&p3.param.1).unwrap()).unwrap();
        let host_signature = SigningKey::<Sha256>::new(host.key().clone()).sign(&server_secret);
        let mut pairing_secret = server_secret.to_vec();
        pairing_secret.extend_from_slice(&host_signature.to_vec());

        let (state, p4) = state
            .on_pairing_secret(&hex_encode(&pairing_secret))
            .unwrap();
        // Host phase 4: recompute the client hash and verify the client signature.
        let cps = hex_decode(&p4.param.1).unwrap();
        let (secret, signature) = cps.split_at(16);
        let mut hasher = Sha256::new();
        hasher.update(server_challenge);
        hasher.update(cert_signature_bits(identity.cert_der()).unwrap());
        hasher.update(secret);
        assert_eq!(
            hasher.finalize().as_slice(),
            client_hash.as_slice(),
            "host-side clienthash check would fail — phase 3/4 layout mismatch"
        );
        let client_public = cert_rsa_public_key(identity.cert_der()).unwrap();
        VerifyingKey::<Sha256>::new(client_public)
            .verify(secret, &Signature::try_from(signature).unwrap())
            .expect("host-side clientpairingsecret verify would fail");

        let paired = state.finish(true).unwrap();
        assert_eq!(paired.server_cert_der, host.cert_der());
    }

    #[test]
    fn a_wrong_pin_is_reported_as_wrong_pin_not_mitm() {
        let identity = ClientIdentity::generate().unwrap();
        let host = ClientIdentity::generate().unwrap();
        let seed = PairingSeed {
            salt: SALT,
            challenge: *b"CLIENT CHALLENGE",
            secret: [7u8; 16],
        };
        // Client thinks the PIN is 5338; host heard 0000, so the host derives a
        // different AES key and its phase-2 hash covers a garbled challenge.
        let (state, _p1) = start(&identity, PIN, seed);
        let host_key = {
            let mut hasher = Sha256::new();
            hasher.update(SALT);
            hasher.update(b"0000");
            let d = hasher.finalize();
            let mut k = [0u8; 16];
            k.copy_from_slice(&d[..16]);
            k
        };
        let (state, p2) = state
            .on_server_cert(&hex_encode(host.cert_pem().as_bytes()))
            .unwrap();
        let garbled = ecb_decrypt(&host_key, &hex_decode(&p2.param.1).unwrap()).unwrap();
        let host_sig = cert_signature_bits(host.cert_der()).unwrap();
        let server_secret = [9u8; 16];
        let mut hasher = Sha256::new();
        hasher.update(&garbled);
        hasher.update(&host_sig);
        hasher.update(server_secret);
        let mut plaintext = hasher.finalize().to_vec();
        plaintext.extend_from_slice(b"SERVER CHALLENGE");
        // The host encrypts with *its* key; we decrypt with ours. Real Sunshine would
        // send exactly these bytes.
        let challengeresponse = hex_encode(&ecb_encrypt(&host_key, &plaintext).unwrap());
        let (state, _p3) = state.on_challenge_response(&challengeresponse).unwrap();
        let host_signature = SigningKey::<Sha256>::new(host.key().clone()).sign(&server_secret);
        let mut pairing_secret = server_secret.to_vec();
        pairing_secret.extend_from_slice(&host_signature.to_vec());
        // The signature itself is honest, so this must be WrongPin, not Pairing.
        match state.on_pairing_secret(&hex_encode(&pairing_secret)) {
            Err(GameStreamError::WrongPin) => {}
            other => panic!("expected WrongPin, got {other:?}"),
        }
    }

    #[test]
    fn a_forged_host_signature_is_a_mitm_error() {
        let identity = ClientIdentity::generate().unwrap();
        let host = ClientIdentity::generate().unwrap();
        let imposter = ClientIdentity::generate().unwrap();
        let seed = PairingSeed {
            salt: SALT,
            challenge: [1u8; 16],
            secret: [2u8; 16],
        };
        let key = sunshine_aes_key();
        let (state, _p1) = start(&identity, PIN, seed);
        let (state, _p2) = state
            .on_server_cert(&hex_encode(host.cert_pem().as_bytes()))
            .unwrap();
        let mut plaintext = [0u8; 48].to_vec();
        plaintext[32..].copy_from_slice(b"SERVER CHALLENGE");
        let (state, _p3) = state
            .on_challenge_response(&hex_encode(&ecb_encrypt(&key, &plaintext).unwrap()))
            .unwrap();
        // Signed by the wrong key: must fail the MITM check before any PIN check.
        let server_secret = [9u8; 16];
        let forged = SigningKey::<Sha256>::new(imposter.key().clone()).sign(&server_secret);
        let mut pairing_secret = server_secret.to_vec();
        pairing_secret.extend_from_slice(&forged.to_vec());
        match state.on_pairing_secret(&hex_encode(&pairing_secret)) {
            Err(GameStreamError::Pairing(msg)) => {
                assert!(msg.contains("man-in-the-middle"), "wrong failure: {msg}");
            }
            other => panic!("expected a MITM pairing error, got {other:?}"),
        }
    }

    #[test]
    fn hex_helpers_round_trip_and_reject_garbage() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x0a]), "00ff0a");
        assert_eq!(hex_decode("00FF0a").unwrap(), vec![0x00, 0xff, 0x0a]);
        assert!(hex_decode("0g").is_none());
        assert!(hex_decode("abc").is_none());
    }
}
