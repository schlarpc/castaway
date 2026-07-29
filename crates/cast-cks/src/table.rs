//! The checked-in signature table — the offline fallback.
//!
//! 900 two-day windows, 2023-01-01 through 2027-12-06, two precomputed
//! signatures each (SHA-1 and SHA-256), over a peer certificate re-issued per
//! window from one template and one key. Every window points at the *same* peer
//! certificate and key; only the signatures differ.
//!
//! Provenance and the caveats that come with it are in `fixtures/README.md`.
//! The one that matters operationally: **the table stops on 2027-12-06**, and
//! after that only the network path can produce a credential.

use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::EncodePrivateKey as _;
use rsa::RsaPrivateKey;

use crate::pem;
use crate::template::PeerTemplate;
use crate::window::{Window, WINDOW_SECS};
use crate::{CastCredential, CksError, CredentialOrigin};

/// Start of window 0: 2023-01-01T00:00:00Z.
const EPOCH_UNIX: i64 = 1_672_531_200;

/// Number of windows in the shipped table.
const WINDOW_COUNT: u32 = 900;

/// Bytes per precomputed signature (RSA-2048).
const SIGNATURE_LEN: usize = 256;

const DEVICE_CERT_PEM: &str = include_str!("../fixtures/device_cert.pem");
const ICA_PEM: &str = include_str!("../fixtures/ica.pem");
const PEER_TEMPLATE_DER: &[u8] = include_bytes!("../fixtures/peer_template.der");
const PEER_KEY_DER: &[u8] = include_bytes!("../fixtures/peer_key.der");
const SIGNATURES_SHA1: &[u8] = include_bytes!("../fixtures/signatures_sha1.bin");
const SIGNATURES_SHA256: &[u8] = include_bytes!("../fixtures/signatures_sha256.bin");

/// The embedded table, parsed.
#[derive(Debug, Clone)]
pub struct StaticTable {
    template: PeerTemplate,
    peer_key: RsaPrivateKey,
    peer_key_pkcs8_der: Vec<u8>,
    device_cert_der: Vec<u8>,
    ica_der: Vec<u8>,
}

impl StaticTable {
    /// Parse the embedded fixtures.
    ///
    /// # Errors
    /// [`CksError::Pem`], [`CksError::InvalidKey`] or [`CksError::Template`] if a
    /// fixture does not have the shape it is supposed to. All inputs are compile-
    /// time constants, so this either always succeeds or always fails — the tests
    /// below are what make that a checked property rather than a hope.
    pub fn load() -> Result<Self, CksError> {
        if SIGNATURES_SHA1.len() != WINDOW_COUNT as usize * SIGNATURE_LEN
            || SIGNATURES_SHA256.len() != WINDOW_COUNT as usize * SIGNATURE_LEN
        {
            return Err(CksError::Table(format!(
                "signature tables are {} and {} bytes; {WINDOW_COUNT} windows needs {}",
                SIGNATURES_SHA1.len(),
                SIGNATURES_SHA256.len(),
                WINDOW_COUNT as usize * SIGNATURE_LEN
            )));
        }

        let peer_key = RsaPrivateKey::from_pkcs1_der(PEER_KEY_DER)
            .map_err(|e| CksError::InvalidKey(format!("embedded peer key: {e}")))?;
        let peer_key_pkcs8_der = peer_key
            .to_pkcs8_der()
            .map_err(|e| CksError::InvalidKey(format!("re-encoding the peer key: {e}")))?
            .as_bytes()
            .to_vec();

        let window0 = Window::new(EPOCH_UNIX, EPOCH_UNIX + WINDOW_SECS)?;
        Ok(Self {
            template: PeerTemplate::parse(PEER_TEMPLATE_DER.to_vec(), window0)?,
            peer_key,
            peer_key_pkcs8_der,
            device_cert_der: pem::decode_one(DEVICE_CERT_PEM, "CERTIFICATE")?,
            ica_der: pem::decode_one(ICA_PEM, "CERTIFICATE")?,
        })
    }

    /// The window at `index`, if the table covers it.
    #[must_use]
    pub fn window(&self, index: u32) -> Option<Window> {
        if index >= WINDOW_COUNT {
            return None;
        }
        let start = EPOCH_UNIX + i64::from(index) * WINDOW_SECS;
        Window::new(start, start + WINDOW_SECS).ok()
    }

    /// The index of the window covering `unix`, if the table reaches that far.
    #[must_use]
    pub fn index_at(&self, unix: i64) -> Option<u32> {
        if unix < EPOCH_UNIX {
            return None;
        }
        let index = u32::try_from((unix - EPOCH_UNIX) / WINDOW_SECS).ok()?;
        (index < WINDOW_COUNT).then_some(index)
    }

    /// The last instant the table covers, exclusive.
    #[must_use]
    pub const fn covers_until(&self) -> i64 {
        EPOCH_UNIX + WINDOW_COUNT as i64 * WINDOW_SECS
    }

    /// Build the credential for the window covering `unix`.
    ///
    /// # Errors
    /// [`CksError::OutOfRange`] if the table does not cover `unix` — which, past
    /// 2027-12-06, it never will. [`CksError::Sign`] if re-issuing fails.
    pub fn credential_at(&self, unix: i64) -> Result<CastCredential, CksError> {
        let index = self.index_at(unix).ok_or(CksError::OutOfRange {
            unix,
            covers_until: self.covers_until(),
        })?;
        let window = self.window(index).ok_or(CksError::OutOfRange {
            unix,
            covers_until: self.covers_until(),
        })?;
        let at = index as usize * SIGNATURE_LEN;
        CastCredential::new(
            self.device_cert_der.clone(),
            vec![self.ica_der.clone()],
            self.template.reissue(window, &self.peer_key)?,
            self.peer_key_pkcs8_der.clone(),
            SIGNATURES_SHA1[at..at + SIGNATURE_LEN].to_vec(),
            SIGNATURES_SHA256[at..at + SIGNATURE_LEN].to_vec(),
            window,
            CredentialOrigin::StaticTable { index },
        )
    }

    /// The parsed template, for tests.
    #[cfg(test)]
    pub(crate) fn template(&self) -> &PeerTemplate {
        &self.template
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use sha1::Sha1;
    use sha2::{Digest as _, Sha256};

    fn table() -> &'static StaticTable {
        static T: std::sync::OnceLock<StaticTable> = std::sync::OnceLock::new();
        T.get_or_init(|| StaticTable::load().unwrap())
    }

    fn device_key() -> RsaPublicKey {
        use rsa::pkcs8::DecodePublicKey as _;
        let (_, cert) = x509_parser::parse_x509_certificate(&table().device_cert_der).unwrap();
        RsaPublicKey::from_public_key_der(cert.public_key().raw).unwrap()
    }

    #[test]
    fn the_table_covers_the_range_it_is_documented_to() {
        let t = table();
        assert_eq!(t.window(0).unwrap().start_unix(), 1_672_531_200); // 2023-01-01
        assert_eq!(t.window(899).unwrap().end_unix(), 1_828_051_200); // 2027-12-06
        assert!(t.window(900).is_none());
        assert_eq!(t.covers_until(), 1_828_051_200);
    }

    #[test]
    fn window_lookup_agrees_with_window_bounds() {
        let t = table();
        for index in [0, 1, 652, 899] {
            let w = t.window(index).unwrap();
            assert_eq!(t.index_at(w.start_unix()), Some(index));
            assert_eq!(t.index_at(w.end_unix() - 1), Some(index));
        }
        assert_eq!(t.index_at(1_672_531_199), None, "before the table starts");
        assert_eq!(t.index_at(1_828_051_200), None, "the instant it ends");
    }

    /// The golden vectors: SHA-256 of the re-issued peer certificate for four
    /// windows, taken from the reference extractor. If the DER rewrite drifts by a
    /// byte these change, and every signature in the table stops verifying.
    #[test]
    fn reissued_certificates_match_the_reference_bytes() {
        let t = table();
        for (index, want) in [
            (
                0u32,
                "3eb84f0318dbfd33185b3ee5eecc757e37e3b44e78bbbea33c4e8bf45a0caf0e",
            ),
            (
                1,
                "30e74921b6fa2eb98ce860563363d074b41330cfd6feff9f3c6c6a194760a830",
            ),
            (
                652,
                "15f9611b2e90c2315673515b60484ec3f7f13ba30711678f996fad1b592d1a1d",
            ),
            (
                899,
                "a3065249fee57cdfbc228c008a938bebe2c95a49f3d030d23631fb84b21e5919",
            ),
        ] {
            let der = t
                .template
                .reissue(t.window(index).unwrap(), &t.peer_key)
                .unwrap();
            assert_eq!(hex(&Sha256::digest(&der)), want, "window {index}");
        }
    }

    /// The property the whole fallback rests on: for every window, both shipped
    /// signatures verify against the shipped device certificate, over the
    /// certificate this crate re-issues. 1800 RSA verifications.
    #[test]
    fn every_shipped_signature_verifies_over_the_certificate_we_reissue() {
        let t = table();
        let key = device_key();
        let sha1_vk = VerifyingKey::<Sha1>::new(key.clone());
        let sha256_vk = VerifyingKey::<Sha256>::new(key);
        for index in 0..WINDOW_COUNT {
            let cred = t
                .credential_at(t.window(index).unwrap().start_unix())
                .unwrap();
            let der = cred.peer_cert_der();
            assert!(
                sha1_vk
                    .verify(
                        der,
                        &Signature::try_from(cred.signature(crate::HashAlgo::Sha1)).unwrap()
                    )
                    .is_ok(),
                "window {index} SHA-1"
            );
            assert!(
                sha256_vk
                    .verify(
                        der,
                        &Signature::try_from(cred.signature(crate::HashAlgo::Sha256)).unwrap()
                    )
                    .is_ok(),
                "window {index} SHA-256"
            );
        }
    }

    /// A signature only covers its own window's certificate — which is why the
    /// provider has to re-resolve when the window rolls rather than caching one
    /// credential forever.
    #[test]
    fn a_signature_does_not_verify_against_a_neighbouring_window() {
        let t = table();
        let vk = VerifyingKey::<Sha256>::new(device_key());
        let cred0 = t.credential_at(t.window(0).unwrap().start_unix()).unwrap();
        let cred1 = t.credential_at(t.window(1).unwrap().start_unix()).unwrap();
        let sig0 = Signature::try_from(cred0.signature(crate::HashAlgo::Sha256)).unwrap();
        assert!(vk.verify(cred1.peer_cert_der(), &sig0).is_err());
    }

    #[test]
    fn past_the_end_of_the_table_is_a_typed_error_not_a_wrong_credential() {
        let t = table();
        match t.credential_at(t.covers_until()) {
            Err(CksError::OutOfRange { covers_until, .. }) => {
                assert_eq!(covers_until, 1_828_051_200);
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn the_reissued_certificate_asserts_its_windows_validity() {
        let t = table();
        let w = t.window(652).unwrap();
        let cred = t.credential_at(w.start_unix()).unwrap();
        let (_, cert) = x509_parser::parse_x509_certificate(cred.peer_cert_der()).unwrap();
        assert_eq!(cert.validity().not_before.timestamp(), w.start_unix());
        assert_eq!(cert.validity().not_after.timestamp(), w.end_unix());
    }

    /// Openscreen rejects a TLS certificate whose `notAfter` is more than four
    /// days out. A 2-day window clears that; a wider one would not.
    #[test]
    fn the_window_fits_openscreens_four_day_cap() {
        let w = table().window(0).unwrap();
        assert!(w.end_unix() - w.start_unix() <= 4 * 86_400);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
