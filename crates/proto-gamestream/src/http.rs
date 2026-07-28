//! The NVHTTP transport — the thin I/O shim under [`crate::nvhttp`].
//!
//! Two things make this more than "issue a GET". First, TLS here is *pinned, not
//! validated*: the host's certificate is self-signed and was handed to us during
//! pairing, so the only correct check is "is this byte-for-byte the certificate we
//! paired with" — a webpki roots check would fail every host, and skipping
//! verification entirely would accept any. Second, our client certificate is presented
//! on every request; it is the credential, and a host that has forgotten it answers
//! `401` with an XML body rather than failing the handshake.
//!
//! Requests are synchronous (`ureq`) run on a blocking thread, matching how the rest
//! of the workspace treats short control-plane HTTP: pairing blocks on a human typing
//! a PIN, so an unbounded wait on a dedicated thread is more honest than an async
//! timeout that has to be disabled anyway.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::error::GameStreamError;
use crate::identity::ClientIdentity;
use crate::nvhttp::{Request, Transport};

/// A client bound to one host.
pub struct NvHttpClient {
    host: String,
    http_port: u16,
    https_port: u16,
    agent_plain: ureq::Agent,
    agent_tls: Option<ureq::Agent>,
}

impl NvHttpClient {
    /// A client that can only speak plain HTTP — enough for `/serverinfo` and the
    /// first four pairing phases, which is all an unpaired host will answer anyway.
    #[must_use]
    pub fn unpaired(host: impl Into<String>, http_port: u16) -> Self {
        Self {
            host: host.into(),
            http_port,
            https_port: crate::nvhttp::DEFAULT_HTTPS_PORT,
            agent_plain: ureq::AgentBuilder::new().build(),
            agent_tls: None,
        }
    }

    /// Add the TLS half: our identity as the client certificate, the host's
    /// certificate as the sole trust anchor.
    ///
    /// # Errors
    /// [`GameStreamError::Http`] if the identity or pinned certificate is unusable.
    pub fn with_tls(
        mut self,
        identity: &ClientIdentity,
        server_cert_der: Vec<u8>,
        https_port: u16,
    ) -> Result<Self, GameStreamError> {
        let key_der = crate::pairing::pem_to_der(identity.key_pem())
            .ok_or_else(|| GameStreamError::Http("client key PEM did not parse".into()))?;
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServerCert {
                expected: server_cert_der.clone(),
            }))
            .with_client_auth_cert(
                vec![CertificateDer::from(identity.cert_der().to_vec())],
                PrivateKeyDer::try_from(key_der)
                    .map_err(|e| GameStreamError::Http(format!("client key: {e}")))?,
            )
            .map_err(|e| GameStreamError::Http(e.to_string()))?;
        self.https_port = https_port;
        self.agent_tls = Some(
            ureq::AgentBuilder::new()
                .tls_config(Arc::new(config))
                .build(),
        );
        Ok(self)
    }

    /// Issue a request, returning the XML body. Blocking — call from
    /// `spawn_blocking`.
    ///
    /// # Errors
    /// [`GameStreamError::Http`] on transport failure or a TLS request made by a
    /// client that was never given an identity.
    pub fn send(&self, request: &Request) -> Result<String, GameStreamError> {
        let (agent, scheme, port) = match request.transport {
            Transport::Plain => (&self.agent_plain, "http", self.http_port),
            Transport::Tls => (
                self.agent_tls
                    .as_ref()
                    .ok_or_else(|| GameStreamError::Http("no TLS identity; pair first".into()))?,
                "https",
                self.https_port,
            ),
        };
        let url = format!(
            "{scheme}://{}:{port}{}",
            bracketed(&self.host),
            request.path_and_query
        );
        match agent.get(&url).call() {
            Ok(response) => response
                .into_string()
                .map_err(|e| GameStreamError::Http(e.to_string())),
            // A 401 carries the XML body that says *why*, so it is not a transport
            // failure — the caller's parser turns it into NotPaired.
            Err(ureq::Error::Status(_, response)) => response
                .into_string()
                .map_err(|e| GameStreamError::Http(e.to_string())),
            Err(e) => Err(GameStreamError::Http(e.to_string())),
        }
    }
}

fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Accepts exactly one certificate: the one pairing pinned.
#[derive(Debug)]
struct PinnedServerCert {
    expected: Vec<u8>,
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Byte equality is the whole check, deliberately. The certificate is
        // self-signed with no name that matches how we dial (an IP), and its validity
        // window is irrelevant to whether it is the host we paired with — Sunshine
        // accepts expired *client* certs for the same reason.
        if end_entity.as_ref() == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "host certificate does not match the one pairing pinned".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
