//! # cast-cks
//!
//! Cast receiver-auth credentials, from the CKS backend or from a checked-in table.
//!
//! ## What problem this solves
//!
//! CASTv2 opens every session with an `AuthChallenge`. A receiver must reply with
//! a Google-issued device certificate chain and a signature over its own TLS
//! certificate. [`crypto_cast_auth`] can *produce* that signature — given a device
//! private key. No software receiver has one: Cast device keys are provisioned
//! into hardware. That is why `castaway` has, until now, only been able to
//! authenticate to senders that trust a locally-generated root, which is none of
//! the official ones (OPEN-QUESTIONS Q2).
//!
//! The mechanism every shipping software receiver uses instead is **replay**, and
//! it turns on one detail of Openscreen's sender:
//!
//! ```text
//! size_t nonce_response_size = nonce_response.size();
//! ErrorOr<std::vector<uint8_t>> nonce_plus_peer_cert_der =
//!     peer_cert.SerializeToDER(nonce_response_size);
//! ```
//!
//! The blob the sender verifies is built from the nonce the receiver **echoes**,
//! not the one the sender issued, and `enforce_nonce_checking` defaults to false.
//! Echo an empty nonce and the signed message is the peer certificate alone — so
//! one signature stays valid for that certificate's entire life, and a signature
//! computed once, elsewhere, works forever after.
//!
//! Receivers exploit that by generating their peer certificate on a fixed 2-day
//! schedule from a fixed key, and shipping a table of one precomputed signature
//! per window. This crate is that mechanism: [`table::StaticTable`] holds 900
//! windows (2023-01-01 → 2027-12-06, 1800 signatures, all verified — see
//! `fixtures/README.md`), and [`api`] speaks to the backend that serves the
//! current window on demand.
//!
//! ## The two invariants
//!
//! Both are easy to violate by accident and produce a receiver that completes its
//! TLS handshake and *then* fails authentication, so both are in the types:
//!
//! 1. **The TLS certificate must be the peer certificate the signature covers.**
//!    [`CastCredential`] owns the certificate, its key and the signatures
//!    together, and hands out the TLS identity only via
//!    [`CastCredential::tls_identity`] — there is no way to take the signature
//!    from one credential and the certificate from another.
//! 2. **The response must echo an empty `sender_nonce`.** A replayed signature
//!    covers the certificate alone; echoing the sender's nonce makes it stop
//!    verifying. [`CastCredential::signed_auth`] returns a
//!    [`crypto_cast_auth::SignedAuth`] whose
//!    [`nonce_echo`](crypto_cast_auth::SignedAuth::nonce_echo) is
//!    [`NonceEcho::Empty`], and the caller that fills the protobuf matches on that
//!    field rather than reaching for the challenge.
//!
//! ## Layering
//!
//! [`api`], [`table`], [`template`] and [`window`] are sans-I/O and synchronous
//! (ground rule 3). [`provider`] is the thin actor that owns the socket, the disk
//! cache and the fallback order.
#![forbid(unsafe_code)]

use thiserror::Error;

pub mod api;
pub mod cache;
mod pem;
pub mod provider;
pub mod table;
pub mod template;
pub mod window;

pub use crypto_cast_auth::{HashAlgo, NonceEcho, SigAlgo, SignedAuth};
pub use provider::{CksConfig, CksProvider};
pub use table::StaticTable;
pub use window::Window;

/// Errors from credential acquisition.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CksError {
    /// A PEM document could not be read.
    #[error("PEM: {0}")]
    Pem(String),

    /// A private key could not be parsed or re-encoded.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// The peer certificate template is not shaped the way re-issuing requires.
    #[error("peer certificate template: {0}")]
    Template(String),

    /// Re-signing the re-issued peer certificate failed.
    #[error("signing the peer certificate: {0}")]
    Sign(String),

    /// The embedded table is malformed.
    #[error("signature table: {0}")]
    Table(String),

    /// A validity window could not be constructed or rendered.
    #[error("validity window: {0}")]
    Window(String),

    /// The static table does not reach the requested time. Past `covers_until`
    /// only the network path can produce a credential.
    #[error(
        "the checked-in signature table stops at {covers_until} and cannot cover {unix}; \
         only the CKS backend can supply a credential past that point"
    )]
    OutOfRange {
        /// The instant a credential was wanted for.
        unix: i64,
        /// The first instant the table does not cover.
        covers_until: i64,
    },

    /// The backend could not be reached, or answered with a transport error.
    #[error("CKS request: {0}")]
    Http(String),

    /// The backend answered, but not with something this crate can use.
    #[error("CKS response: {0}")]
    Response(String),

    /// The on-disk cache could not be read or written.
    #[error("credential cache: {0}")]
    Cache(String),
}

/// Where a credential came from.
///
/// Carried on the credential rather than logged and discarded, because "which
/// identity is this panel presenting, and will it still work next month" is the
/// question that matters when a sender starts refusing to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialOrigin {
    /// Fetched from the CKS backend. Lives as long as the backend does.
    Network,
    /// Read back from the on-disk cache — a previously fetched [`Self::Network`]
    /// credential, still inside its window.
    Cache,
    /// Re-issued from the checked-in table at `index`. Works offline; stops
    /// working when the table runs out.
    StaticTable {
        /// The window's index in the table.
        index: u32,
    },
}

impl core::fmt::Display for CredentialOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Network => f.write_str("CKS backend"),
            Self::Cache => f.write_str("cached CKS response"),
            Self::StaticTable { index } => write!(f, "checked-in table, window {index}"),
        }
    }
}

/// A complete Cast receiver-auth credential for one validity window.
///
/// Holds the TLS identity and the device-auth material as one unit. That is the
/// whole point of the type: the signature covers the certificate, so anything
/// that could pair them wrongly must not be expressible.
#[derive(Debug, Clone)]
pub struct CastCredential {
    device_cert_der: Vec<u8>,
    intermediates_der: Vec<Vec<u8>>,
    peer_cert_der: Vec<u8>,
    peer_key_pkcs8_der: Vec<u8>,
    sha1_signature: Vec<u8>,
    sha256_signature: Vec<u8>,
    window: Window,
    origin: CredentialOrigin,
}

impl CastCredential {
    /// Assemble a credential, checking the parts agree.
    ///
    /// # Errors
    /// [`CksError::Response`] if a signature is not the 256 bytes an RSA-2048
    /// device key produces, or the certificate chain is empty.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_cert_der: Vec<u8>,
        intermediates_der: Vec<Vec<u8>>,
        peer_cert_der: Vec<u8>,
        peer_key_pkcs8_der: Vec<u8>,
        sha1_signature: Vec<u8>,
        sha256_signature: Vec<u8>,
        window: Window,
        origin: CredentialOrigin,
    ) -> Result<Self, CksError> {
        for (what, sig) in [("SHA-1", &sha1_signature), ("SHA-256", &sha256_signature)] {
            if sig.len() != 256 {
                return Err(CksError::Response(format!(
                    "{what} signature is {} bytes; an RSA-2048 device signature is 256",
                    sig.len()
                )));
            }
        }
        if device_cert_der.is_empty() || peer_cert_der.is_empty() {
            return Err(CksError::Response(
                "credential is missing a certificate".into(),
            ));
        }
        Ok(Self {
            device_cert_der,
            intermediates_der,
            peer_cert_der,
            peer_key_pkcs8_der,
            sha1_signature,
            sha256_signature,
            window,
            origin,
        })
    }

    /// The TLS server identity this credential *requires* the receiver to present:
    /// the peer certificate DER and its PKCS#8 private key.
    ///
    /// Presenting anything else breaks device auth, because the signature covers
    /// exactly these bytes. Taking both from one call is what makes that hard to
    /// get wrong.
    #[must_use]
    pub fn tls_identity(&self) -> (&[u8], &[u8]) {
        (&self.peer_cert_der, &self.peer_key_pkcs8_der)
    }

    /// The peer certificate DER — the message the signatures are over.
    #[must_use]
    pub fn peer_cert_der(&self) -> &[u8] {
        &self.peer_cert_der
    }

    /// The Google device certificate, DER — `AuthResponse.client_auth_certificate`.
    #[must_use]
    pub fn device_cert_der(&self) -> &[u8] {
        &self.device_cert_der
    }

    /// The intermediate certificates, DER, that chain the device certificate to a
    /// Cast root.
    #[must_use]
    pub fn intermediates_der(&self) -> &[Vec<u8>] {
        &self.intermediates_der
    }

    /// The precomputed signature for `hash`.
    #[must_use]
    pub fn signature(&self, hash: HashAlgo) -> &[u8] {
        match hash {
            HashAlgo::Sha1 => &self.sha1_signature,
            HashAlgo::Sha256 => &self.sha256_signature,
        }
    }

    /// The device-auth response for a challenge requesting `hash`.
    ///
    /// The returned [`SignedAuth`] carries [`NonceEcho::Empty`]: this signature was
    /// computed over the peer certificate alone, so the response must echo nothing.
    #[must_use]
    pub fn signed_auth(&self, hash: HashAlgo) -> SignedAuth {
        SignedAuth {
            signature: self.signature(hash).to_vec(),
            client_auth_certificate: self.device_cert_der.clone(),
            intermediate_certificate: self.intermediates_der.clone(),
            hash,
            algorithm: SigAlgo::RsaPkcs1v15,
            nonce_echo: NonceEcho::Empty,
        }
    }

    /// The window this credential is valid for.
    #[must_use]
    pub const fn window(&self) -> Window {
        self.window
    }

    /// Whether the credential covers `unix`.
    #[must_use]
    pub const fn valid_at(&self, unix: i64) -> bool {
        self.window.contains(unix)
    }

    /// Where this credential came from.
    #[must_use]
    pub const fn origin(&self) -> &CredentialOrigin {
        &self.origin
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn credential(origin: CredentialOrigin) -> Result<CastCredential, CksError> {
        CastCredential::new(
            vec![1],
            vec![vec![2]],
            vec![3],
            vec![4],
            vec![0; 256],
            vec![0; 256],
            Window::new(100, 200).unwrap(),
            origin,
        )
    }

    #[test]
    fn a_short_signature_is_rejected_at_construction() {
        let bad = CastCredential::new(
            vec![1],
            vec![],
            vec![3],
            vec![4],
            vec![0; 128],
            vec![0; 256],
            Window::new(100, 200).unwrap(),
            CredentialOrigin::Network,
        );
        assert!(matches!(bad, Err(CksError::Response(_))));
    }

    /// The replay only verifies when nothing is echoed. If this ever became
    /// anything else, senders would silently start rejecting the receiver.
    #[test]
    fn a_replayed_response_echoes_no_nonce() {
        let c = credential(CredentialOrigin::Network).unwrap();
        for hash in [HashAlgo::Sha1, HashAlgo::Sha256] {
            assert_eq!(c.signed_auth(hash).nonce_echo, NonceEcho::Empty);
        }
    }

    #[test]
    fn the_tls_certificate_is_the_certificate_the_signature_covers() {
        let c = credential(CredentialOrigin::Network).unwrap();
        let (cert, _key) = c.tls_identity();
        assert_eq!(cert, c.peer_cert_der());
    }

    #[test]
    fn origin_renders_for_an_operator() {
        assert_eq!(
            CredentialOrigin::StaticTable { index: 652 }.to_string(),
            "checked-in table, window 652"
        );
        assert_eq!(CredentialOrigin::Network.to_string(), "CKS backend");
    }
}
