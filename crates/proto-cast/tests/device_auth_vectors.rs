//! Generates the device-auth vectors in `tests/fixtures/device-auth/` and proves the
//! checked-in bytes still match what this receiver produces today.
//!
//! This test on its own only says the vectors are current. What makes them *mean*
//! anything is the `openscreen-device-auth` flake check, which compiles openscreen's own
//! sender-side verifier — the code Chrome runs — and asserts the verdict recorded beside
//! each one. Together they answer a question that had been answered by reading source:
//! exactly where an official sender stops trusting this receiver, and exactly which of
//! the many things it checks we already satisfy.
//!
//! Set `CASTAWAY_BLESS_DEVICE_AUTH_VECTORS=1` to rewrite the fixtures after an
//! intentional change. Read the diff before blessing: every byte here is something a
//! sender inspects.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use crypto_cast_auth::{CastDeviceSigner, DevCredential, HashAlgo};
use prost::Message as _;
use proto_cast::proto::{AuthChallenge, AuthResponse, DeviceAuthMessage, HashAlgorithm};
use proto_cast::session::DeviceAuthResponder as _;
use proto_cast::{CastAuthResponder, TlsIdentity};
use rsa::pkcs8::DecodePrivateKey as _;

/// The clock every vector is generated and verified at. Fixed so the certificate windows
/// in the bytes — and the verdicts about them — do not depend on the day the test runs.
const AT: u64 = 1_800_000_000;

/// The nonce a sender challenges with. Sixteen bytes, as `kNonceSizeInBytes` requires.
const NONCE: &[u8; 16] = b"castaway-nonce16";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/device-auth")
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(AT)
}

fn credential() -> DevCredential {
    let root = rsa::RsaPrivateKey::from_pkcs8_pem(
        &std::fs::read_to_string(fixtures().join("dev-root-key.pem")).unwrap(),
    )
    .unwrap();
    let device = rsa::RsaPrivateKey::from_pkcs8_pem(
        &std::fs::read_to_string(fixtures().join("dev-device-key.pem")).unwrap(),
    )
    .unwrap();
    CastDeviceSigner::dev_from_keys(&root, &device, i64::try_from(AT).unwrap()).unwrap()
}

/// The TLS certificate a sender sees, issued the way the receiver issues its own.
///
/// The fixture key is RSA where the receiver generates ECDSA, for one reason: ECDSA
/// signatures are randomized, so an ECDSA certificate would differ on every run and could
/// not be checked in. Nothing a sender inspects about the peer certificate depends on the
/// key type — it never verifies this certificate's signature at all, only reads its
/// validity window and hashes its DER — so the substitution costs the vectors nothing.
fn peer_cert(at: SystemTime) -> Vec<u8> {
    let key = std::fs::read_to_string(fixtures().join("tls-key.pem")).unwrap();
    let key_der = pem_body(&key);
    TlsIdentity::from_key_at(&key_der, &["castaway.local".to_string()], at)
        .unwrap()
        .cert_der()
}

/// A peer certificate with rcgen's default validity window, which is what this receiver
/// presented before the window was made explicit.
fn peer_cert_unbounded() -> Vec<u8> {
    let key = pem_body(&std::fs::read_to_string(fixtures().join("tls-key.pem")).unwrap());
    let key = rcgen::KeyPair::try_from(key.as_slice()).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["castaway.local".to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "castaway");
    params.self_signed(&key).unwrap().der().to_vec()
}

fn pem_body(pem: &str) -> Vec<u8> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .unwrap()
}

/// One vector: what the sender sees, and what it should conclude.
struct Vector {
    name: &'static str,
    peer_cert: Vec<u8>,
    /// The nonce the sender challenged with — what it will compare the echo against.
    nonce: Vec<u8>,
    auth: DeviceAuthMessage,
    /// The root to evaluate the chain against. `None` means the trust store senders ship,
    /// which is the case that matters.
    anchor: Option<Vec<u8>>,
    at: u64,
    expect: &'static str,
}

fn respond(cred: &DevCredential, peer_cert: &[u8], challenge: AuthChallenge) -> DeviceAuthMessage {
    let responder =
        CastAuthResponder::new(std::sync::Arc::new(cred.signer.clone()), peer_cert.to_vec());
    DeviceAuthMessage {
        challenge: None,
        response: Some(responder.respond(&challenge).unwrap()),
        error: None,
    }
}

fn challenge(nonce: Option<Vec<u8>>, hash: HashAlgorithm) -> AuthChallenge {
    AuthChallenge {
        signature_algorithm: None,
        sender_nonce: nonce,
        hash_algorithm: Some(hash as i32),
    }
}

fn vectors() -> Vec<Vector> {
    let cred = credential();
    let cert = peer_cert(now());
    let root = cred.root_ca_der.clone();

    // Precomputed for the two vectors that do not go through the normal responder: a
    // deliberately wrong signed blob, and a peer certificate with rcgen's default window.
    let signed_without_nonce = cred.signer.sign(&cert, None, HashAlgo::Sha256).unwrap();
    let unbounded = peer_cert_unbounded();

    vec![
        // Trusted: everything but the root is right, and a sender told to trust our root
        // says so. This is the case a provisioned credential turns into the real thing.
        Vector {
            name: "dev-chain-trusted",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &cert,
                challenge(Some(NONCE.to_vec()), HashAlgorithm::Sha256),
            ),
            anchor: Some(root.clone()),
            at: AT,
            expect: "ok",
        },
        // The same bytes against the roots senders actually ship. This is the answer to
        // "why can't Chrome cast to it".
        Vector {
            name: "dev-chain-google-roots",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &cert,
                challenge(Some(NONCE.to_vec()), HashAlgorithm::Sha256),
            ),
            anchor: None,
            at: AT,
            expect: "error kCastV2CertNotSignedByTrustedCa",
        },
        // SHA-1 is still accepted: `enforce_sha256_checking` is off on the path a sender
        // takes, so a legacy digest is not what would be turning anyone away.
        Vector {
            name: "sha1-digest",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &cert,
                challenge(Some(NONCE.to_vec()), HashAlgorithm::Sha1),
            ),
            anchor: Some(root.clone()),
            at: AT,
            expect: "ok",
        },
        // No echo at all. The sender notes it and proceeds, rebuilding the signed blob from
        // the empty echo — so a receiver that never returns the nonce still authenticates.
        Vector {
            name: "nonce-omitted",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(&cred, &cert, challenge(None, HashAlgorithm::Sha256)),
            anchor: Some(root.clone()),
            at: AT,
            expect: "ok",
        },
        // An echo that is not the sender's nonce. Also accepted, for the same reason: the
        // blob is rebuilt from what came back, and the comparison against what went out sets
        // a metric rather than an error.
        Vector {
            name: "nonce-mismatched",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &cert,
                challenge(Some(b"a-different-non".to_vec()), HashAlgorithm::Sha256),
            ),
            anchor: Some(root.clone()),
            at: AT,
            expect: "ok",
        },
        // The negative control. Claim an echo, sign only the certificate: get the signed-blob
        // layout wrong and the sender catches it. Without this the four cases above passing
        // would not establish that the layout is checked at all.
        Vector {
            name: "nonce-not-covered",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: DeviceAuthMessage {
                challenge: None,
                response: Some(AuthResponse {
                    signature: signed_without_nonce.signature.clone(),
                    client_auth_certificate: signed_without_nonce.client_auth_certificate.clone(),
                    intermediate_certificate: signed_without_nonce.intermediate_certificate.clone(),
                    signature_algorithm: None,
                    sender_nonce: Some(NONCE.to_vec()),
                    hash_algorithm: Some(HashAlgorithm::Sha256 as i32),
                    crl: None,
                }),
                error: None,
            },
            anchor: Some(root.clone()),
            at: AT,
            expect: "error kCastV2SignedBlobsMismatch",
        },
        // The regression lock: rcgen's default window, which this receiver used to present.
        // The chain and signature are beside the point — the sender never gets that far.
        Vector {
            name: "tls-cert-unbounded",
            peer_cert: unbounded.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &unbounded,
                challenge(Some(NONCE.to_vec()), HashAlgorithm::Sha256),
            ),
            anchor: Some(root.clone()),
            at: AT,
            expect: "error kCastV2TlsCertValidityPeriodTooLong",
        },
        // The other end of the same rule, and the reason the certificate is reissued rather
        // than minted once: let it lapse and the sender walks away from a healthy panel.
        Vector {
            name: "tls-cert-expired",
            peer_cert: cert.clone(),
            nonce: NONCE.to_vec(),
            auth: respond(
                &cred,
                &cert,
                challenge(Some(NONCE.to_vec()), HashAlgorithm::Sha256),
            ),
            anchor: Some(root),
            at: AT + 30 * 24 * 60 * 60,
            expect: "error kCastV2TlsCertExpired",
        },
    ]
}

#[test]
fn the_checked_in_vectors_still_describe_this_receiver() {
    let bless = std::env::var_os("CASTAWAY_BLESS_DEVICE_AUTH_VECTORS").is_some();
    let root = fixtures();

    for v in vectors() {
        let dir = root.join(v.name);
        let mut files: Vec<(&str, Vec<u8>)> = vec![
            ("peer_cert.der", v.peer_cert),
            ("auth.bin", v.auth.encode_to_vec()),
            ("nonce.bin", v.nonce),
            ("time", v.at.to_string().into_bytes()),
            ("expect", format!("{}\n", v.expect).into_bytes()),
        ];
        if let Some(anchor) = v.anchor {
            files.push(("anchor.der", anchor));
        }

        if bless {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
        }
        for (name, want) in files {
            let path = dir.join(name);
            if bless {
                std::fs::write(&path, &want).unwrap();
                continue;
            }
            let have = std::fs::read(&path).unwrap_or_else(|e| {
                panic!(
                    "{}: {e}. Regenerate with CASTAWAY_BLESS_DEVICE_AUTH_VECTORS=1",
                    path.display()
                )
            });
            assert_eq!(
                have,
                want,
                "{} no longer matches what this receiver produces. The openscreen check \
                 verifies these bytes, so a stale vector means it is verifying a receiver \
                 we do not have. Regenerate with CASTAWAY_BLESS_DEVICE_AUTH_VECTORS=1 and \
                 read the diff.",
                path.display()
            );
        }
    }
}

/// The vectors are only as good as their peer certificate being the one we really serve.
#[test]
fn the_peer_certificate_is_the_one_the_receiver_would_present() {
    let der = peer_cert(now());
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    let at = i64::try_from(AT).unwrap();
    assert!(cert.validity().not_after.timestamp() - at < 4 * 24 * 60 * 60);
    assert!(cert.validity().not_before.timestamp() < at);
}
