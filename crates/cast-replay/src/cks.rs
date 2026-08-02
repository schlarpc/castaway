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
use crate::provider::Identity;
use crate::template::PeerTemplate;
use crate::window::{Window, WINDOW_SECS};
use crate::{CastCredential, CredentialOrigin, ReplayError};

/// Start of window 0: 2023-01-01T00:00:00Z.
const EPOCH_UNIX: i64 = 1_672_531_200;

/// Number of windows in the shipped table.
const WINDOW_COUNT: u32 = 900;

/// Bytes per precomputed signature (RSA-2048).
const SIGNATURE_LEN: usize = 256;

/// The bundled CKS identity: SoftMedia's Google-issued device certificate, its
/// intermediate, the RSA peer key and the certificate template re-issued per window,
/// and 900 windows of precomputed receiver-auth signatures.
///
/// **Not checked in**, for the same reason as [`crate::airserver::BundledIdentity`]:
/// it is someone else's Cast device credential, private key included. It is carved
/// out of `libAirReceiver.so` at build time (`nix/airreceiver-carve.nix`) and reaches
/// the crate through `build.rs`.
#[derive(Clone, Copy, Debug)]
pub struct BundledCks {
    /// The Google-issued device certificate, PEM.
    pub device_cert_pem: &'static str,
    /// The intermediate certificate(s), PEM.
    pub ica_pem: &'static str,
    /// The peer certificate template, DER — re-issued per window.
    pub peer_template_der: &'static [u8],
    /// The RSA-2048 peer private key, PKCS#1 DER.
    pub peer_key_der: &'static [u8],
    /// Per-window SHA-1 receiver-auth signatures, concatenated.
    pub signatures_sha1: &'static [u8],
    /// Per-window SHA-256 receiver-auth signatures, concatenated.
    pub signatures_sha256: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/cks_identity.rs"));

/// Whether this build carries a bundled CKS identity.
#[must_use]
pub const fn has_bundled_identity() -> bool {
    BUNDLED_CKS.is_some()
}

/// The embedded table, parsed.
#[derive(Debug, Clone)]
pub struct CksTable {
    template: PeerTemplate,
    peer_key: RsaPrivateKey,
    peer_key_pkcs8_der: Vec<u8>,
    device_cert_der: Vec<u8>,
    ica_der: Vec<u8>,
    identity: BundledCks,
}

impl CksTable {
    /// Parse the embedded fixtures.
    ///
    /// # Errors
    /// [`ReplayError::NoIdentity`] on a build without the carved identity, or
    /// [`ReplayError::Pem`], [`ReplayError::InvalidKey`] or [`ReplayError::Template`]
    /// if a fixture does not have the shape it is supposed to. Given an identity all
    /// inputs are compile-time constants, so this either always succeeds or always
    /// fails — the tests below are what make that a checked property rather than a hope.
    pub fn load() -> Result<Self, ReplayError> {
        let id = BUNDLED_CKS.ok_or(ReplayError::NoIdentity {
            identity: Identity::Cks,
        })?;
        if id.signatures_sha1.len() != WINDOW_COUNT as usize * SIGNATURE_LEN
            || id.signatures_sha256.len() != WINDOW_COUNT as usize * SIGNATURE_LEN
        {
            return Err(ReplayError::Table(format!(
                "signature tables are {} and {} bytes; {WINDOW_COUNT} windows needs {}",
                id.signatures_sha1.len(),
                id.signatures_sha256.len(),
                WINDOW_COUNT as usize * SIGNATURE_LEN
            )));
        }

        let peer_key = RsaPrivateKey::from_pkcs1_der(id.peer_key_der)
            .map_err(|e| ReplayError::InvalidKey(format!("embedded peer key: {e}")))?;
        let peer_key_pkcs8_der = peer_key
            .to_pkcs8_der()
            .map_err(|e| ReplayError::InvalidKey(format!("re-encoding the peer key: {e}")))?
            .as_bytes()
            .to_vec();

        let window0 = Window::new(EPOCH_UNIX, EPOCH_UNIX + WINDOW_SECS)?;
        Ok(Self {
            template: PeerTemplate::parse(id.peer_template_der.to_vec(), window0)?,
            peer_key,
            peer_key_pkcs8_der,
            device_cert_der: pem::decode_one(id.device_cert_pem, "CERTIFICATE")?,
            ica_der: pem::decode_one(id.ica_pem, "CERTIFICATE")?,
            identity: id,
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
    /// [`ReplayError::OutOfRange`] if the table does not cover `unix` — which, past
    /// 2027-12-06, it never will. [`ReplayError::Sign`] if re-issuing fails.
    pub fn credential_at(&self, unix: i64) -> Result<CastCredential, ReplayError> {
        let index = self.index_at(unix).ok_or(ReplayError::OutOfRange {
            identity: Identity::Cks,
            unix,
            covers_until: self.covers_until(),
        })?;
        let window = self.window(index).ok_or(ReplayError::OutOfRange {
            identity: Identity::Cks,
            unix,
            covers_until: self.covers_until(),
        })?;
        let at = index as usize * SIGNATURE_LEN;
        CastCredential::new(
            self.device_cert_der.clone(),
            vec![self.ica_der.clone()],
            self.template.reissue(window, &self.peer_key)?,
            self.peer_key_pkcs8_der.clone(),
            self.identity.signatures_sha1[at..at + SIGNATURE_LEN].to_vec(),
            self.identity.signatures_sha256[at..at + SIGNATURE_LEN].to_vec(),
            window,
            CredentialOrigin::CksTable { index },
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

    fn table() -> &'static CksTable {
        static T: std::sync::OnceLock<CksTable> = std::sync::OnceLock::new();
        T.get_or_init(|| CksTable::load().unwrap())
    }

    fn device_key() -> RsaPublicKey {
        use rsa::pkcs8::DecodePublicKey as _;
        let (_, cert) = x509_parser::parse_x509_certificate(&table().device_cert_der).unwrap();
        RsaPublicKey::from_public_key_der(cert.public_key().raw).unwrap()
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn the_table_covers_the_range_it_is_documented_to() {
        let t = table();
        assert_eq!(t.window(0).unwrap().start_unix(), 1_672_531_200); // 2023-01-01
        assert_eq!(t.window(899).unwrap().end_unix(), 1_828_051_200); // 2027-12-06
        assert!(t.window(900).is_none());
        assert_eq!(t.covers_until(), 1_828_051_200);
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
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
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
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
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
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
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn a_signature_does_not_verify_against_a_neighbouring_window() {
        let t = table();
        let vk = VerifyingKey::<Sha256>::new(device_key());
        let cred0 = t.credential_at(t.window(0).unwrap().start_unix()).unwrap();
        let cred1 = t.credential_at(t.window(1).unwrap().start_unix()).unwrap();
        let sig0 = Signature::try_from(cred0.signature(crate::HashAlgo::Sha256)).unwrap();
        assert!(vk.verify(cred1.peer_cert_der(), &sig0).is_err());
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn past_the_end_of_the_table_is_a_typed_error_not_a_wrong_credential() {
        let t = table();
        match t.credential_at(t.covers_until()) {
            Err(ReplayError::OutOfRange { covers_until, .. }) => {
                assert_eq!(covers_until, 1_828_051_200);
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
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
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn the_window_fits_openscreens_four_day_cap() {
        let w = table().window(0).unwrap();
        assert!(w.end_unix() - w.start_unix() <= 4 * 86_400);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
