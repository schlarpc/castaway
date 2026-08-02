//! The Cast trust anchors, so a CRL check can cover the whole chain a sender validates.
//!
//! Two static, public, self-signed certificates — the entirety of Chromium's Cast trust
//! store. Neither is secret and neither is ours; they are here because
//! [`crate::crl::CastCrl::revokes`] needs the certificate *above* the topmost one we
//! present, and without it the chain we check is one shorter than the chain the sender
//! checks.
//!
//! Why that gap matters. `revokes` looks a serial range up under the issuer's key hash to
//! revoke the subordinate below it, so the last certificate in the list never has its own
//! serial tested and the anchor's key hash is never tested at all. What we present tops
//! out one below the root in both identities — `Eureka Gen1 ICA` (serial 1) for CKS,
//! `Widevine Cast Subroot` (serial 0x0142) for AirServer — while the sender runs
//! `CheckRevocation` over `GetBestValidPath()->certs`, which includes the anchor from its
//! own store. A range published under either root's key covering those serials would be
//! invisible here and fatal there: `servable_for` would hand back a `ServableCrl`, the
//! receiver would attach to its `AuthResponse` the very document that makes a
//! Chromium-based sender answer `ERR_CERTS_REVOKED`, and the D41 "only warning we get"
//! line would never fire.
//!
//! Both DERs are from openscreen's `cast/common/certificate/` — `cast_root_ca_cert_der`
//! and `eureka_root_ca_der` — and both were checked against the chains this crate ships:
//! each root verifies the topmost certificate of its identity.

use x509_parser::prelude::FromDer as _;

/// `CN=Cast Root CA`, serial 2, valid to 2034-03-28. Anchors the AirServer chain.
const CAST_ROOT_CA: &[u8] = include_bytes!("../fixtures/roots/cast_root_ca.der");

/// `CN=Eureka Root CA`, serial 1, valid to 2032-12-12. Anchors the CKS chain.
const EUREKA_ROOT_CA: &[u8] = include_bytes!("../fixtures/roots/eureka_root_ca.der");

/// Every anchor Chromium trusts for Cast.
pub const ALL: [&[u8]; 2] = [CAST_ROOT_CA, EUREKA_ROOT_CA];

/// The anchor that issued `top`, if it is one we hold.
///
/// Matched on the issuer's raw distinguished name against each anchor's subject, which is
/// the same key the sender's path builder uses. `None` for a chain rooted somewhere we do
/// not know about — a future identity, or a certificate that does not parse. The caller
/// then checks the chain it has, which is what it did before this existed.
#[must_use]
pub fn anchor_for(top: &[u8]) -> Option<&'static [u8]> {
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(top).ok()?;
    let issuer = cert.tbs_certificate.issuer.as_raw();
    ALL.iter().copied().find(|anchor| {
        x509_parser::certificate::X509Certificate::from_der(anchor)
            .is_ok_and(|(_, root)| root.tbs_certificate.subject.as_raw() == issuer)
    })
}

/// `chain` with its trust anchor appended, ready for [`crate::crl::CastCrl::revokes`].
///
/// The chain `revokes` documents wanting — leaf first, anchor last. Appends nothing when
/// the anchor is unknown or already the top, so this is safe to call on any chain.
#[must_use]
pub fn with_anchor<'a>(chain: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let mut out = chain.to_vec();
    if let Some(top) = chain.last() {
        // Both anchors are self-signed, so a chain that already ends at one would match
        // itself and be duplicated — which `revokes` would then read as the root having
        // issued itself.
        if let Some(anchor) = anchor_for(top).filter(|anchor| anchor != top) {
            out.push(anchor);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pem_der(pem: &str) -> Vec<u8> {
        crate::pem::decode_one(pem, "CERTIFICATE").expect("a valid certificate")
    }

    #[test]
    #[cfg_attr(
        not(airserver_identity),
        ignore = "needs the carved AirServer identity"
    )]
    fn each_shipped_chain_finds_its_own_anchor() {
        // The whole point: both identities top out one below the root, and which root it
        // is differs between them. Getting this wrong is silent — the chain still checks,
        // just one certificate short of what the sender checks.
        let ica = pem_der(
            crate::cks::BUNDLED_CKS
                .expect("carved CKS identity")
                .ica_pem,
        );
        assert_eq!(anchor_for(&ica), Some(EUREKA_ROOT_CA), "CKS -> Eureka Root");

        let subroot: &[u8] = crate::airserver::BUNDLED
            .expect("carved AirServer identity")
            .chain_der[1];
        assert_eq!(
            anchor_for(subroot),
            Some(CAST_ROOT_CA),
            "AirServer -> Cast Root"
        );
    }

    #[test]
    fn a_root_is_not_appended_to_itself_and_a_stranger_gets_nothing() {
        // Both roots are self-signed, so issuer == subject: naive matching would append
        // each one to itself forever.
        for root in ALL {
            assert_eq!(
                anchor_for(root),
                Some(root),
                "self-signed: issuer is itself"
            );
            assert_eq!(with_anchor(&[root]), vec![root], "and is not duplicated");
        }
        assert_eq!(anchor_for(b"not a certificate"), None);
        assert_eq!(with_anchor(&[b"not a certificate"]).len(), 1);
    }

    #[test]
    #[cfg_attr(not(cks_identity), ignore = "needs the carved CKS identity")]
    fn with_anchor_extends_a_real_chain_by_exactly_one() {
        let ica = pem_der(
            crate::cks::BUNDLED_CKS
                .expect("carved CKS identity")
                .ica_pem,
        );
        let leaf = pem_der(
            crate::cks::BUNDLED_CKS
                .expect("carved CKS identity")
                .device_cert_pem,
        );
        let chain: Vec<&[u8]> = vec![&leaf, &ica];
        let extended = with_anchor(&chain);
        assert_eq!(extended.len(), 3);
        assert_eq!(extended[2], EUREKA_ROOT_CA);
    }
}
