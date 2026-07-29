//! A second offline receiver-auth identity, from AirServer's bundled database.
//!
//! [`crate::table`] holds SoftMedia's (AirReceiver's) identity. This holds App
//! Dynamic's (AirServer's), and the reason to carry both is **not** horizon — the
//! CKS table runs to 2027-12-06 and this one stops on 2027-03-21, so on expiry
//! alone this is strictly the worse of the two. The reason is *revocation*.
//!
//! D41 names the risk it could not mitigate: the AirReceiver identity is shared
//! with every install of that app, `AuthResponse` carries a `crl` field, Chrome
//! fetches the Cast device CRL, and Google can revoke it with no warning and no
//! way for the receiver to see it coming. A second identity does not remove that
//! risk, but it means the response is a config change rather than a dead panel —
//! and this identity is genuinely independent, not a second leaf off the same
//! branch:
//!
//! | | CKS ([`crate::table`]) | AirServer (here) |
//! |---|---|---|
//! | device CN | `RYW0O FA8FCA6AC5A0` | `2001805200936810051` |
//! | issuer | `Eureka Gen1 ICA` | `NVidia mdarcy … Cast ICA` |
//! | root path | `Eureka Root CA` | `Widevine Cast Subroot` |
//! | covers | 2023-01-01 → 2027-12-06 | 2024-03-20 → 2027-03-21 |
//!
//! Different device, different intermediate, different branch of the Cast PKI. A
//! revocation or root-level problem that kills one has no particular reason to
//! kill the other.
//!
//! ## Why the certificates are stored rather than re-issued
//!
//! [`crate::table`] ships one peer-certificate *template* and re-issues it per
//! window, because CKS's certificates differ only in their validity dates. That
//! trick does not work here. AirServer's per-window certificates differ in three
//! places, and the third is fatal to it:
//!
//! * **serial** — linear in the window index (`259_286_400 + index * 86_400`),
//! * **validity** — linear in the window index,
//! * **subject CN** — a *random UUID per window* (`CN=c1cafd47-251c-…`).
//!
//! The UUID is not derivable from anything, and the device signature covers the
//! certificate's exact DER, so a certificate we rebuilt would be rejected. The
//! 1095 certificates are therefore checked in verbatim, 738 bytes each at a fixed
//! stride. This costs ~790 KiB and buys the absence of a whole class of silent
//! failure, which is the trade ground rule 1 asks for.
//!
//! ## Windows overlap here, unlike CKS
//!
//! CKS steps 2 days with 2-day validity, so its windows tile. AirServer steps
//! **1 day** with **2-day validity**, so at any instant *two* windows are valid.
//! [`AirServerTable::index_at`] returns the later one — the one that started
//! today — because it has the most remaining life, which minimises the chance of
//! a roll landing mid-session.
//!
//! Provenance, and the tool that regenerates these fixtures, are in
//! `fixtures/airserver/README.md`.

use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::EncodePrivateKey as _;
use rsa::RsaPrivateKey;

use crate::window::Window;
use crate::{CastCredential, CksError, CredentialOrigin};

/// Start of window 0: 2024-03-20T00:00:00Z.
const EPOCH_UNIX: i64 = 1_710_892_800;

/// Distance between consecutive window *starts*. One day — not the validity.
const STEP_SECS: i64 = 86_400;

/// How long each window is valid for. Two days, so windows overlap by one.
const VALIDITY_SECS: i64 = 172_800;

/// Number of windows in the bundled database.
const WINDOW_COUNT: u32 = 1095;

/// Bytes per peer certificate. Uniform across every window in this database,
/// which is what makes a fixed-stride blob correct; the exporter refuses to
/// write one if that ever stops being true.
const PEER_CERT_STRIDE: usize = 738;

/// Bytes per precomputed signature (RSA-2048).
const SIGNATURE_LEN: usize = 256;

const DEVICE_CERT_DER: &[u8] = include_bytes!("../fixtures/airserver/airserver_device_crt.der");
const CHAIN0_DER: &[u8] = include_bytes!("../fixtures/airserver/airserver_chain0.der");
const CHAIN1_DER: &[u8] = include_bytes!("../fixtures/airserver/airserver_chain1.der");
const PEER_KEY_DER: &[u8] = include_bytes!("../fixtures/airserver/airserver_peer_key.der");
const PEER_CERTS: &[u8] = include_bytes!("../fixtures/airserver/airserver_peer_certs.bin");
const SIGNATURES_SHA1: &[u8] = include_bytes!("../fixtures/airserver/airserver_sha1.bin");
const SIGNATURES_SHA256: &[u8] = include_bytes!("../fixtures/airserver/airserver_sha256.bin");

/// The bundled AirServer identity, parsed.
#[derive(Debug, Clone)]
pub struct AirServerTable {
    peer_key_pkcs8_der: Vec<u8>,
    device_cert_der: Vec<u8>,
    chain_der: Vec<Vec<u8>>,
}

impl AirServerTable {
    /// Parse the embedded fixtures.
    ///
    /// # Errors
    /// [`CksError::Table`] if a fixture is not the length the layout requires, or
    /// [`CksError::InvalidKey`] if the peer key does not parse. Every input is a
    /// compile-time constant, so this either always succeeds or always fails — the
    /// tests are what turn that into a checked property.
    pub fn load() -> Result<Self, CksError> {
        let expect_certs = WINDOW_COUNT as usize * PEER_CERT_STRIDE;
        let expect_sigs = WINDOW_COUNT as usize * SIGNATURE_LEN;
        if PEER_CERTS.len() != expect_certs {
            return Err(CksError::Table(format!(
                "AirServer peer certificates are {} bytes; {WINDOW_COUNT} windows at \
                 stride {PEER_CERT_STRIDE} needs {expect_certs}",
                PEER_CERTS.len()
            )));
        }
        if SIGNATURES_SHA1.len() != expect_sigs || SIGNATURES_SHA256.len() != expect_sigs {
            return Err(CksError::Table(format!(
                "AirServer signature tables are {} and {} bytes; {WINDOW_COUNT} windows needs {expect_sigs}",
                SIGNATURES_SHA1.len(),
                SIGNATURES_SHA256.len()
            )));
        }

        let peer_key = RsaPrivateKey::from_pkcs1_der(PEER_KEY_DER)
            .map_err(|e| CksError::InvalidKey(format!("embedded AirServer peer key: {e}")))?;
        let peer_key_pkcs8_der = peer_key
            .to_pkcs8_der()
            .map_err(|e| CksError::InvalidKey(format!("re-encoding the AirServer peer key: {e}")))?
            .as_bytes()
            .to_vec();

        Ok(Self {
            peer_key_pkcs8_der,
            device_cert_der: DEVICE_CERT_DER.to_vec(),
            chain_der: vec![CHAIN0_DER.to_vec(), CHAIN1_DER.to_vec()],
        })
    }

    /// The window at `index`, if the table covers it.
    #[must_use]
    pub fn window(&self, index: u32) -> Option<Window> {
        if index >= WINDOW_COUNT {
            return None;
        }
        let start = EPOCH_UNIX + i64::from(index) * STEP_SECS;
        Window::new(start, start + VALIDITY_SECS).ok()
    }

    /// The index of the best window covering `unix`, if any does.
    ///
    /// Windows overlap, so up to two qualify; this returns the later one, which has
    /// the most remaining life. Past the last window's *start* but inside its
    /// validity, the last window is still the answer — which is why this clamps
    /// before checking containment rather than rejecting an out-of-range index.
    #[must_use]
    pub fn index_at(&self, unix: i64) -> Option<u32> {
        if unix < EPOCH_UNIX {
            return None;
        }
        let raw = (unix - EPOCH_UNIX) / STEP_SECS;
        let index = u32::try_from(raw).ok()?.min(WINDOW_COUNT - 1);
        self.window(index)
            .filter(|w| w.contains(unix))
            .map(|_| index)
    }

    /// The last instant this table covers, exclusive.
    #[must_use]
    pub const fn covers_until(&self) -> i64 {
        EPOCH_UNIX + (WINDOW_COUNT as i64 - 1) * STEP_SECS + VALIDITY_SECS
    }

    /// The first instant this table covers.
    #[must_use]
    pub const fn covers_from(&self) -> i64 {
        EPOCH_UNIX
    }

    /// Build the credential for the window covering `unix`.
    ///
    /// # Errors
    /// [`CksError::OutOfRange`] if this table does not reach `unix`.
    pub fn credential_at(&self, unix: i64) -> Result<CastCredential, CksError> {
        let index = self.index_at(unix).ok_or(CksError::OutOfRange {
            unix,
            covers_until: self.covers_until(),
        })?;
        let window = self.window(index).ok_or(CksError::OutOfRange {
            unix,
            covers_until: self.covers_until(),
        })?;

        let cert_at = index as usize * PEER_CERT_STRIDE;
        let sig_at = index as usize * SIGNATURE_LEN;
        CastCredential::new(
            self.device_cert_der.clone(),
            self.chain_der.clone(),
            PEER_CERTS[cert_at..cert_at + PEER_CERT_STRIDE].to_vec(),
            self.peer_key_pkcs8_der.clone(),
            SIGNATURES_SHA1[sig_at..sig_at + SIGNATURE_LEN].to_vec(),
            SIGNATURES_SHA256[sig_at..sig_at + SIGNATURE_LEN].to_vec(),
            window,
            CredentialOrigin::AirServerTable { index },
        )
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
    use sha2::Sha256;

    fn table() -> &'static AirServerTable {
        static T: std::sync::OnceLock<AirServerTable> = std::sync::OnceLock::new();
        T.get_or_init(|| AirServerTable::load().unwrap())
    }

    fn device_key() -> RsaPublicKey {
        use rsa::pkcs8::DecodePublicKey as _;
        let (_, cert) = x509_parser::parse_x509_certificate(DEVICE_CERT_DER).unwrap();
        RsaPublicKey::from_public_key_der(cert.public_key().raw).unwrap()
    }

    #[test]
    fn the_fixtures_load() {
        let _ = table();
    }

    #[test]
    fn the_table_covers_the_range_it_is_documented_to() {
        let t = table();
        // 2024-03-20 .. 2024-03-22
        assert_eq!(t.window(0).unwrap().start_unix(), 1_710_892_800);
        assert_eq!(t.window(0).unwrap().end_unix(), 1_711_065_600);
        // 2027-03-19 .. 2027-03-21
        assert_eq!(t.window(1094).unwrap().start_unix(), 1_805_414_400);
        assert_eq!(t.window(1094).unwrap().end_unix(), 1_805_587_200);
        assert!(t.window(1095).is_none());
        assert_eq!(t.covers_until(), 1_805_587_200);
    }

    /// The property that separates this table from the CKS one. If these ever
    /// stopped overlapping, `index_at` would be picking between windows that do
    /// not exist.
    #[test]
    fn consecutive_windows_overlap_by_one_day() {
        let t = table();
        let a = t.window(10).unwrap();
        let b = t.window(11).unwrap();
        assert_eq!(b.start_unix() - a.start_unix(), STEP_SECS);
        assert!(b.start_unix() < a.end_unix(), "windows must overlap");
        assert!(a.contains(b.start_unix()) && b.contains(b.start_unix()));
    }

    /// Two windows cover any given instant; the later one is the useful one.
    #[test]
    fn lookup_prefers_the_window_with_more_life_left() {
        let t = table();
        let day10 = EPOCH_UNIX + 10 * STEP_SECS;
        // Window 9 also covers this instant, but ends a day sooner.
        assert!(t.window(9).unwrap().contains(day10));
        assert_eq!(t.index_at(day10), Some(10));
    }

    #[test]
    fn the_tail_of_the_last_window_still_resolves() {
        let t = table();
        // Past the last window's start, inside its validity: a naive index
        // computation lands on 1095, which does not exist.
        let late = EPOCH_UNIX + 1094 * STEP_SECS + STEP_SECS + 1;
        assert!(late > t.window(1094).unwrap().start_unix());
        assert_eq!(t.index_at(late), Some(1094));
        assert!(t.credential_at(late).is_ok());
    }

    #[test]
    fn outside_the_range_is_out_of_range_not_a_wrong_credential() {
        let t = table();
        assert_eq!(t.index_at(EPOCH_UNIX - 1), None);
        assert_eq!(t.index_at(t.covers_until()), None);
        assert!(matches!(
            t.credential_at(t.covers_until()),
            Err(CksError::OutOfRange { .. })
        ));
    }

    /// The load-bearing test. Every signature must verify, under the device
    /// certificate's own public key, over the stored peer certificate — which is
    /// exactly what a sender recomputes. A mis-sliced blob or an off-by-one in the
    /// stride shows up here and nowhere else until a sender refuses to connect.
    ///
    /// Sampled rather than exhaustive to keep the suite fast; the exporter verifies
    /// all 1095 and `fixtures/airserver/README.md` records that.
    #[test]
    fn stored_signatures_verify_over_stored_certificates() {
        let t = table();
        let key = device_key();
        let sha1: VerifyingKey<Sha1> = VerifyingKey::new(key.clone());
        let sha256: VerifyingKey<Sha256> = VerifyingKey::new(key);

        for index in [0_u32, 1, 2, 547, 1093, 1094] {
            let window = t.window(index).unwrap();
            let credential = t.credential_at(window.start_unix()).unwrap();
            let cert = credential.peer_cert_der();

            sha1.verify(
                cert,
                &Signature::try_from(credential.signature(crate::HashAlgo::Sha1)).unwrap(),
            )
            .unwrap_or_else(|e| panic!("SHA-1 signature for window {index} does not verify: {e}"));
            sha256
                .verify(
                    cert,
                    &Signature::try_from(credential.signature(crate::HashAlgo::Sha256)).unwrap(),
                )
                .unwrap_or_else(|e| {
                    panic!("SHA-256 signature for window {index} does not verify: {e}")
                });
        }
    }

    /// The subject CN really is a fresh UUID per window — the fact that rules out
    /// template re-issue. Documented as a test so that if a future database drops
    /// the UUID, the cheaper representation becomes discoverable rather than
    /// staying a paragraph of prose nobody rechecks.
    #[test]
    fn each_window_has_a_distinct_random_subject() {
        let t = table();
        let mut seen = std::collections::HashSet::new();
        for index in [0_u32, 1, 2, 3, 500, 1094] {
            let w = t.window(index).unwrap();
            let credential = t.credential_at(w.start_unix()).unwrap();
            let (_, cert) =
                x509_parser::parse_x509_certificate(credential.peer_cert_der()).unwrap();
            let subject = cert.subject().to_string();
            assert!(seen.insert(subject), "window {index} reuses a subject");
        }
    }

    /// The identity is a different device on a different branch from the CKS one,
    /// which is the entire reason this module exists.
    #[test]
    fn the_identity_is_not_the_cks_one() {
        let (_, device) = x509_parser::parse_x509_certificate(DEVICE_CERT_DER).unwrap();
        assert!(
            device.subject().to_string().contains("2001805200936810051"),
            "unexpected device subject: {}",
            device.subject()
        );
        let (_, subroot) = x509_parser::parse_x509_certificate(CHAIN1_DER).unwrap();
        assert!(
            subroot
                .subject()
                .to_string()
                .contains("Widevine Cast Subroot"),
            "unexpected chain root: {}",
            subroot.subject()
        );
    }

    /// A replayed signature is only valid with an empty nonce echo, whichever
    /// identity produced it.
    #[test]
    fn a_replayed_response_echoes_no_nonce() {
        let t = table();
        let c = t.credential_at(EPOCH_UNIX).unwrap();
        for hash in [crate::HashAlgo::Sha1, crate::HashAlgo::Sha256] {
            assert_eq!(c.signed_auth(hash).nonce_echo, crate::NonceEcho::Empty);
        }
    }
}
