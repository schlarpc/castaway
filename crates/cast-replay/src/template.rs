//! Re-issuing the peer certificate for a window.
//!
//! The static table ships **one** peer certificate, carrying window 0's validity,
//! plus one signature per window. That only works because the certificate for
//! window *n* is derived from the template deterministically: rewrite the two
//! `UTCTime` fields, re-sign the `tbsCertificate` with the peer key. The
//! precomputed signature covers the result, so the derivation has to be
//! byte-exact — reproducing it approximately makes every signature fail, not
//! some of them.
//!
//! This mirrors `FUN_00420848` in `libAirReceiver.so` 5.1.7, which does the same
//! rewrite with `X509_time_adj_ex` before presenting the certificate.

use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use sha2::Sha256;

use crate::window::{UtcTime, Window};
use crate::ReplayError;

/// The DER of the `AlgorithmIdentifier` + `BIT STRING` header that sits between
/// the `tbsCertificate` and the signature.
///
/// Checked rather than skipped, because it pins the digest: these bytes say
/// `sha256WithRSAEncryption`, and re-signing with anything else would produce a
/// certificate whose contents contradict its own algorithm field.
const SIGNATURE_ALGORITHM_TAIL: &[u8] = &[
    0x30, 0x0d, // SEQUENCE, 13 bytes
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01,
    0x0b, // OID 1.2.840.113549.1.1.11
    0x05, 0x00, // NULL parameters
    0x03, 0x82, 0x01, 0x01, 0x00, // BIT STRING, 257 bytes, 0 unused bits
];

/// Length of the RSA-2048 signature the tail declares.
const SIGNATURE_LEN: usize = 256;

/// A peer certificate template with its mutable regions located.
///
/// Parsed once from the shipped DER rather than carrying hardcoded offsets, so a
/// template swap is caught at parse time instead of producing a certificate that
/// silently fails to match its signature.
#[derive(Debug, Clone)]
pub struct PeerTemplate {
    der: Vec<u8>,
    tbs: core::ops::Range<usize>,
    not_before: usize,
    not_after: usize,
}

impl PeerTemplate {
    /// Locate the rewritable regions of a peer certificate template.
    ///
    /// `window0` is the validity the template itself carries; its two rendered
    /// `UTCTime` values are what the fields are found by, which ties the offsets
    /// to the actual bytes rather than to a constant that could drift.
    ///
    /// # Errors
    /// [`ReplayError::Template`] if the DER does not have the shape every shipped
    /// template has: a two-byte-length outer `SEQUENCE`, a two-byte-length
    /// `tbsCertificate`, exactly one occurrence of each `UTCTime`, and a
    /// `sha256WithRSAEncryption` tail followed by 256 signature bytes.
    pub fn parse(der: Vec<u8>, window0: Window) -> Result<Self, ReplayError> {
        // Outer SEQUENCE, long-form length on two bytes.
        if der.len() < 8 || der[0] != 0x30 || der[1] != 0x82 {
            return Err(ReplayError::Template(
                "peer template is not a DER SEQUENCE with a two-byte length".into(),
            ));
        }
        let declared = 4 + usize::from(u16::from_be_bytes([der[2], der[3]]));
        if declared != der.len() {
            return Err(ReplayError::Template(format!(
                "peer template length {declared} disagrees with its {} bytes",
                der.len()
            )));
        }

        // tbsCertificate, likewise a two-byte-length SEQUENCE, starting immediately.
        if der[4] != 0x30 || der[5] != 0x82 {
            return Err(ReplayError::Template(
                "peer template's tbsCertificate is not a DER SEQUENCE with a two-byte length"
                    .into(),
            ));
        }
        let tbs_len = 4 + usize::from(u16::from_be_bytes([der[6], der[7]]));
        let tbs = 4..(4 + tbs_len);
        if tbs.end + SIGNATURE_ALGORITHM_TAIL.len() + SIGNATURE_LEN != der.len() {
            return Err(ReplayError::Template(format!(
                "peer template's tbsCertificate ends at {}, leaving {} bytes for a {}-byte \
                 algorithm identifier and a {SIGNATURE_LEN}-byte signature",
                tbs.end,
                der.len() - tbs.end,
                SIGNATURE_ALGORITHM_TAIL.len()
            )));
        }
        if &der[tbs.end..tbs.end + SIGNATURE_ALGORITHM_TAIL.len()] != SIGNATURE_ALGORITHM_TAIL {
            return Err(ReplayError::Template(
                "peer template is not signed with sha256WithRSAEncryption".into(),
            ));
        }

        let (nb, na) = window0.utc_times()?;
        let not_before = find_unique(&der[tbs.clone()], nb.as_bytes(), "notBefore")? + tbs.start;
        let not_after = find_unique(&der[tbs.clone()], na.as_bytes(), "notAfter")? + tbs.start;
        if not_after <= not_before {
            return Err(ReplayError::Template(
                "peer template's notAfter precedes its notBefore".into(),
            ));
        }

        Ok(Self {
            der,
            tbs,
            not_before,
            not_after,
        })
    }

    /// Re-issue the certificate for `window`, signing with `peer_key`.
    ///
    /// # Errors
    /// [`ReplayError::Window`] if the window cannot be rendered as `UTCTime`,
    /// [`ReplayError::Sign`] if signing fails.
    pub fn reissue(
        &self,
        window: Window,
        peer_key: &RsaPrivateKey,
    ) -> Result<Vec<u8>, ReplayError> {
        let (nb, na) = window.utc_times()?;
        let mut der = self.der.clone();
        der[self.not_before..self.not_before + UtcTime::LEN].copy_from_slice(nb.as_bytes());
        der[self.not_after..self.not_after + UtcTime::LEN].copy_from_slice(na.as_bytes());

        // Self-signed: rewriting the validity invalidates the trailing signature,
        // so it has to be recomputed. Patching the template bytes alone is the
        // difference between one window verifying and all 900 failing.
        let signature = SigningKey::<Sha256>::new(peer_key.clone())
            .try_sign(&der[self.tbs.clone()])
            .map_err(|e| ReplayError::Sign(e.to_string()))?
            .to_vec();
        if signature.len() != SIGNATURE_LEN {
            return Err(ReplayError::Sign(format!(
                "peer key produced a {}-byte signature; the certificate has room for \
                 {SIGNATURE_LEN}",
                signature.len()
            )));
        }
        let at = der.len() - SIGNATURE_LEN;
        der[at..].copy_from_slice(&signature);
        Ok(der)
    }
}

/// Find `needle` in `haystack`, requiring exactly one occurrence.
///
/// Uniqueness is the point: a `UTCTime` that appeared twice would mean the offset
/// picked is ambiguous, and the wrong copy would be rewritten.
fn find_unique(haystack: &[u8], needle: &[u8], what: &str) -> Result<usize, ReplayError> {
    let mut found = None;
    for (i, chunk) in haystack.windows(needle.len()).enumerate() {
        if chunk == needle {
            if found.is_some() {
                return Err(ReplayError::Template(format!(
                    "peer template contains more than one copy of its {what}"
                )));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| {
        ReplayError::Template(format!(
            "peer template does not contain the {what} its window declares"
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::cks::CksTable;

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn template_layout_matches_the_shipped_certificate() {
        let table = CksTable::load().unwrap();
        let t = table.template();
        // Verified against the reference extractor: 734 bytes, tbs at 4..458,
        // the two UTCTime fields at 87 and 102.
        assert_eq!(t.der.len(), 734);
        assert_eq!(t.tbs, 4..458);
        assert_eq!(t.not_before, 87);
        assert_eq!(t.not_after, 102);
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn a_truncated_template_is_rejected_rather_than_reissued() {
        let table = CksTable::load().unwrap();
        let mut der = table.template().der.clone();
        der.truncate(der.len() - 1);
        let w = table.window(0).unwrap();
        assert!(PeerTemplate::parse(der, w).is_err());
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn a_template_signed_with_the_wrong_algorithm_is_rejected() {
        let table = CksTable::load().unwrap();
        let mut der = table.template().der.clone();
        // Flip the digest OID's last byte: 0x0b (sha256) -> 0x05 (sha1).
        let at = table.template().tbs.end + 12;
        assert_eq!(der[at], 0x0b);
        der[at] = 0x05;
        let w = table.window(0).unwrap();
        assert!(PeerTemplate::parse(der, w).is_err());
    }
}
