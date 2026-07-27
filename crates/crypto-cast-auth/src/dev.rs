//! The development credential — a self-signed dev root and a device certificate under
//! it, issued with the extensions a Cast sender's certificate path builder requires.
//!
//! This exists so that "we do not have a Google-rooted credential" is the *only* thing
//! wrong with our device-auth response. Everything else a sender checks — that the chain
//! parses, that the leaf carries `digitalSignature` key usage, that the issuer is a CA
//! with `keyCertSign`, that the signature algorithm is one of the two it accepts, that
//! the chain is ordered leaf-first — is exercised for real against this credential by the
//! `openscreen-device-auth` check. A placeholder byte string, which is what this used to
//! be, makes all of that untestable: a sender rejects it at the parse step and every
//! later requirement stays a guess.
//!
//! The constraints encoded below come from openscreen's
//! `cast/common/certificate/boringssl_trust_store.cc`, which is the same path builder
//! Chrome runs.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SerialNumber, PKCS_RSA_SHA256,
};
use rsa::RsaPrivateKey;
use time::{Duration, OffsetDateTime};

use crate::{CastAuthError, CastDeviceSigner};

/// How long a dev device certificate is valid. Long, deliberately: unlike the *TLS*
/// certificate — which a sender treats as the device-auth signature's expiry and caps at
/// four days — the device certificate is a durable identity, and real Cast device certs
/// outlive the hardware.
const DEVICE_CERT_LIFETIME: Duration = Duration::days(3650);

/// Backdating for `notBefore`, so a sender whose clock trails ours does not see a
/// certificate that is not valid yet.
const BACKDATE: Duration = Duration::hours(24);

/// A locally generated development credential.
///
/// [`DevCredential::root_ca_der`] is the reason this is not a real credential: a sender
/// would have to be told to trust it, and no sender in the room will be.
pub struct DevCredential {
    /// The signer holding the device key and the chain it presents.
    pub signer: CastDeviceSigner,
    /// The dev root the chain terminates in, DER.
    pub root_ca_der: Vec<u8>,
}

/// Issue a dev root and a device certificate under it, both keyed by the RSA keys given.
///
/// Taking the keys rather than generating them keeps this deterministic: with fixed keys
/// and a fixed `now_unix`, the DER is byte-identical every run, which is what lets the
/// device-auth vectors be checked in and compared.
pub(crate) fn issue(
    root_key: &RsaPrivateKey,
    device_key: &RsaPrivateKey,
    now_unix: i64,
) -> Result<DevCredential, CastAuthError> {
    let now = OffsetDateTime::from_unix_timestamp(now_unix)
        .map_err(|e| CastAuthError::DevCert(e.to_string()))?;
    let not_before = now - BACKDATE;
    let not_after = now + DEVICE_CERT_LIFETIME;

    let root_pair = key_pair(root_key)?;
    let device_pair = key_pair(device_key)?;

    // The root. `keyCertSign` is not decoration: the path builder rejects an issuer whose
    // key usage extension is present but lacks that bit, and rejects one with no
    // `basicConstraints` CA bit at all.
    let mut root_params =
        CertificateParams::new(Vec::new()).map_err(|e| CastAuthError::DevCert(e.to_string()))?;
    root_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(1));
    root_params
        .distinguished_name
        .push(DnType::CommonName, "castaway development root");
    root_params
        .distinguished_name
        .push(DnType::OrganizationName, "castaway");
    root_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    root_params.key_usages.push(KeyUsagePurpose::CrlSign);
    root_params.serial_number = Some(SerialNumber::from(1u64));
    root_params.not_before = not_before;
    root_params.not_after = not_after;

    let root_cert = root_params
        .self_signed(&root_pair)
        .map_err(|e| CastAuthError::DevCert(e.to_string()))?;
    let root_der = root_cert.der().to_vec();
    let issuer = Issuer::new(root_params, root_pair);

    // The device certificate. `digitalSignature` is mandatory on the target — a sender
    // refuses a leaf without a key usage extension outright, and refuses one whose key
    // usage omits that bit, both before it ever looks at the signature.
    let mut device_params =
        CertificateParams::new(Vec::new()).map_err(|e| CastAuthError::DevCert(e.to_string()))?;
    device_params
        .distinguished_name
        .push(DnType::CommonName, "castaway development device");
    device_params
        .distinguished_name
        .push(DnType::OrganizationName, "castaway");
    device_params
        .key_usages
        .push(KeyUsagePurpose::DigitalSignature);
    device_params.use_authority_key_identifier_extension = true;
    device_params.serial_number = Some(SerialNumber::from(2u64));
    device_params.not_before = not_before;
    device_params.not_after = not_after;

    let device_cert = device_params
        .signed_by(&device_pair, &issuer)
        .map_err(|e| CastAuthError::DevCert(e.to_string()))?;

    Ok(DevCredential {
        signer: CastDeviceSigner::new(device_key.clone(), device_cert.der().to_vec(), Vec::new()),
        root_ca_der: root_der,
    })
}

/// Hand an RSA key to rcgen. `PKCS_RSA_SHA256` is not a preference: the path builder
/// accepts `sha1WithRSAEncryption` and `sha256WithRSAEncryption` and nothing else, so an
/// ECDSA certificate — rcgen's default — would be rejected for its signature algorithm.
fn key_pair(key: &RsaPrivateKey) -> Result<KeyPair, CastAuthError> {
    let der = CastDeviceSigner::pkcs8_der(key)?;
    KeyPair::from_pkcs8_der_and_sign_algo(&der.into(), &PKCS_RSA_SHA256)
        .map_err(|e| CastAuthError::DevCert(e.to_string()))
}
