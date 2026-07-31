//! Bridges the pure [`crypto_cast_auth`] signer to the [`DeviceAuthResponder`] the
//! session calls. Lives here (not in `crypto-cast-auth`) so the dependency flows
//! `proto-cast` → `crypto-cast-auth`, never the reverse.

use std::sync::Arc;

use crypto_cast_auth::{CastDeviceSigner, HashAlgo, SigAlgo, SignedAuth};

use crate::error::CastError;
use crate::proto::{AuthChallenge, AuthResponse, HashAlgorithm, SignatureAlgorithm};
use crate::session::DeviceAuthResponder;

/// The hash a challenge asks for. The proto2 default is SHA-1; an explicit
/// SHA-256 request is honoured.
pub(crate) fn requested_hash(challenge: &AuthChallenge) -> HashAlgo {
    match challenge.hash_algorithm {
        Some(h) if h == HashAlgorithm::Sha256 as i32 => HashAlgo::Sha256,
        _ => HashAlgo::Sha1,
    }
}

/// Fill the `AuthResponse` proto from signed material.
///
/// The one place the response is built, so the `sender_nonce` rule is applied
/// once rather than at each responder. That field comes from
/// [`SignedAuth::nonce_echo`] — what the signature actually covers — and never
/// from the challenge: a replayed signature covers the peer certificate alone,
/// and echoing the sender's nonce back would make the sender rebuild a different
/// message and reject a response that is otherwise correct.
pub(crate) fn auth_response(signed: SignedAuth) -> AuthResponse {
    let hash_algorithm = match signed.hash {
        HashAlgo::Sha1 => HashAlgorithm::Sha1,
        HashAlgo::Sha256 => HashAlgorithm::Sha256,
    };
    let signature_algorithm = match signed.algorithm {
        SigAlgo::RsaPkcs1v15 => SignatureAlgorithm::RsassaPkcs1v15,
    };
    AuthResponse {
        signature: signed.signature,
        client_auth_certificate: signed.client_auth_certificate,
        intermediate_certificate: signed.intermediate_certificate,
        signature_algorithm: Some(signature_algorithm as i32),
        sender_nonce: signed.nonce_echo.as_bytes().map(<[u8]>::to_vec),
        hash_algorithm: Some(hash_algorithm as i32),
        // No CRL — which is *supposed* to be fine, and currently is not, for a reason
        // that is Chrome's bug rather than our omission.
        //
        // The rule, from `cast_cert_validator.cc` (Chromium 148, unchanged on main).
        // Chrome verifies at `CRLPolicy::CRL_REQUIRED_WITH_FALLBACK`, and a receiver that
        // sends no CRL is meant to be carried by Chrome's built-in *fallback* CRL:
        //
        //     if (fallback_crl) { ...revocation check... }
        //     else if (!crl)    { return ERR_FALLBACK_CRL_INVALID; }
        //     if (!crl)         { return OK_FALLBACK_CRL; }
        //
        // and `AuthResult::success()` counts `ERROR_CRL_OK_FALLBACK_CRL` as success. So
        // the intended path for a CRL-less receiver — ours, and every third-party one —
        // is `OK_FALLBACK_CRL`, and this field is genuinely optional.
        //
        // Which is what **Google Chrome 150 does**: it accepts this receiver with this
        // field empty — "Auth challenge verification succeeded", no CRL diagnostics at
        // all. So on the browser that matters, no CRL is needed and none is sent.
        //
        // **Chromium 148 rejects the same receiver.** Same box, same binary of ours,
        // minutes apart, this field empty in both runs:
        //
        //     Chromium 148  0 auth successes, 4 auth failures, 13 "CRL - Not time-valid"
        //     Chrome   150  1 auth success,   0 auth failures,  0 "CRL - Not time-valid"
        //
        // There its fallback CRL fails to verify, `fallback_crl` is null, and the middle
        // branch returns `ERR_FALLBACK_CRL_INVALID` — "Failed to provide a valid fallback
        // CRL." → `AUTHENTICATION_ERROR`, dropped and retried forever. Supplying a CRL
        // borrowed from a real Cast device (they answer the same challenge with a
        // 3619-byte one, which is why hardware never takes this path) makes Chromium
        // accept us too. So the field is a working lever if we ever need it.
        //
        // What is *not* established is why the two differ, and it is worth being honest
        // that the obvious explanation is wrong. The fallback CRL is a constant compiled
        // into the binary, and it decodes to `not_before 2023-08-04, not_after
        // 2023-08-12` — expired three years ago — but it is byte-identical across
        // branch-heads 7778 (148), 7871 (150) and main, and the full 1791-byte blob is
        // present in the shipped Chrome 150 binary. `cast_crl.cc` and `cast_auth_util.cc`
        // are identical between the two branches, and `cast_cert_validator.cc` is
        // restructured but returns `ERR_FALLBACK_CRL_INVALID` for this case in both. By
        // the public source both builds should refuse us; one does not. Something
        // build-specific is doing it, and this comment does not know what.
        //
        // The earlier version of this comment read that as "every CRL-less software
        // receiver is dead in Chrome, AirReceiver included". That is false — Chrome is
        // exactly where it works. Kept as a note because it was a satisfying theory that
        // survived reading the source and died to one measurement.
        //
        // Left absent, then, on the strength of the measurement rather than the theory:
        // Chrome does not need it, and filling it in costs a Google-signed blob valid for
        // about a week (the captured one ran 2026-07-28 → 2026-08-05) or a live source,
        // which is a cloud dependency in D30's sense. See OPEN-QUESTIONS/D41.
        //
        // openscreen's verifier defaults to `kCrlOptional` and has no fallback CRL at
        // all, which is why the `openscreen-device-auth` vectors judge this response `ok`
        // while Chrome refuses it. The vectors are not wrong about the signature; they
        // simply cannot see this.
        crl: None,
    }
}

/// A per-connection auth responder: the shared device signer plus this connection's
/// TLS server certificate (which the challenge is signed over).
pub struct CastAuthResponder {
    signer: Arc<CastDeviceSigner>,
    tls_cert_der: Vec<u8>,
}

impl CastAuthResponder {
    /// Bind the shared `signer` to one connection's `tls_cert_der`.
    #[must_use]
    pub fn new(signer: Arc<CastDeviceSigner>, tls_cert_der: Vec<u8>) -> Self {
        Self {
            signer,
            tls_cert_der,
        }
    }
}

impl DeviceAuthResponder for CastAuthResponder {
    fn respond(&self, challenge: &AuthChallenge) -> Result<AuthResponse, CastError> {
        let signed = self
            .signer
            .sign(
                &self.tls_cert_der,
                challenge.sender_nonce.as_deref(),
                requested_hash(challenge),
            )
            .map_err(|e| CastError::Auth(e.to_string()))?;
        Ok(auth_response(signed))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use prost::Message as _;

    #[test]
    fn responder_produces_a_signed_response() {
        let signer = Arc::new(CastDeviceSigner::generate_dev().unwrap().signer);
        let responder = CastAuthResponder::new(signer, b"tls-cert".to_vec());
        let challenge = AuthChallenge {
            signature_algorithm: None,
            sender_nonce: Some(vec![1, 2, 3, 4]),
            hash_algorithm: Some(HashAlgorithm::Sha256 as i32),
        };
        let resp = responder.respond(&challenge).unwrap();
        assert!(!resp.signature.is_empty());
        assert_eq!(resp.sender_nonce, Some(vec![1, 2, 3, 4]));
        // It encodes into a DeviceAuthMessage cleanly.
        let dam = crate::proto::DeviceAuthMessage {
            challenge: None,
            response: Some(resp),
            error: None,
        };
        let mut buf = Vec::new();
        dam.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());
    }
}
