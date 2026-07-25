//! The CASTv2 socket actor (OPEN-QUESTIONS Q15): the thin async shell around the pure
//! [`CastSession`]. It owns the TLS listener and one task per sender connection, and
//! does exactly three things per frame — decode, hand to the session, write back what
//! the session says to write. No protocol decisions live here (ground rule 3).
//!
//! Senders reach us over TLS with a certificate they never validate: CASTv2
//! authenticates the *device*, not the transport. The binding between the two is the
//! device-auth handshake, which signs over this connection's TLS certificate — so the
//! actor keeps its certificate DER and hands it to [`CastAuthResponder`] per connection.

use std::net::SocketAddr;
use std::sync::Arc;

use castaway_core::{
    Advertisement, CoreError, ProtocolKind, SessionEvent, SessionSink, SourceAdapter,
};
use crypto_cast_auth::CastDeviceSigner;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::auth::CastAuthResponder;
use crate::error::CastError;
use crate::session::CastSession;
use crate::{framing, CAST_PORT, CAST_SERVICE_TYPE};

/// Cap on a single CASTv2 frame. Real messages are a few KiB; the length prefix is
/// attacker-controlled, so it gets a bound rather than a `Vec` that grows to whatever a
/// sender claims.
const MAX_FRAME: usize = 1 << 20;

/// The receiver's TLS identity: a self-signed certificate plus its key.
///
/// Self-signed is correct here, not a shortcut — every Cast receiver ships one, and
/// senders don't build a chain to a trust root. What matters is that the same DER bytes
/// the sender sees are the bytes the device-auth signature covers.
pub struct TlsIdentity {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

impl TlsIdentity {
    /// Generate a fresh self-signed identity for `subject_alt_names`.
    ///
    /// # Errors
    /// [`CastError::Tls`] if certificate generation fails.
    pub fn self_signed(subject_alt_names: &[String]) -> Result<Self, CastError> {
        let key = rcgen::generate_simple_self_signed(subject_alt_names.to_vec())
            .map_err(|e| CastError::Tls(e.to_string()))?;
        let key_der = PrivateKeyDer::try_from(key.signing_key.serialize_der())
            .map_err(|e| CastError::Tls(e.to_string()))?;
        Ok(Self {
            cert_der: key.cert.der().clone(),
            key_der,
        })
    }

    /// The certificate DER — what the device-auth response signs over.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        self.cert_der.as_ref()
    }

    fn server_config(&self) -> Result<rustls::ServerConfig, CastError> {
        // Name the provider rather than taking `ServerConfig::builder()`'s process-default
        // path: that one *panics* if no default is installed and the crate features are
        // ambiguous, and a library crate doesn't get to panic (ground rule 7).
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| CastError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![self.cert_der.clone()], self.key_der.clone_key())
        .map_err(|e| CastError::Tls(e.to_string()))
    }
}

/// A listening CASTv2 receiver: one TLS listener, one [`CastSession`] per connection.
pub struct CastReceiver {
    listen: SocketAddr,
    friendly_name: String,
    device_id: String,
    acceptor: TlsAcceptor,
    tls_cert_der: Vec<u8>,
    signer: Option<Arc<CastDeviceSigner>>,
}

impl CastReceiver {
    /// Build a receiver listening on `listen` with the given TLS `identity`.
    ///
    /// # Errors
    /// [`CastError::Tls`] if the identity can't be turned into a rustls server config.
    pub fn new(
        listen: SocketAddr,
        friendly_name: impl Into<String>,
        device_id: impl Into<String>,
        identity: &TlsIdentity,
    ) -> Result<Self, CastError> {
        let config = identity.server_config()?;
        Ok(Self {
            listen,
            friendly_name: friendly_name.into(),
            device_id: device_id.into(),
            acceptor: TlsAcceptor::from(Arc::new(config)),
            tls_cert_der: identity.cert_der().to_vec(),
            signer: None,
        })
    }

    /// Answer device-auth challenges with `signer` instead of refusing them. Without a
    /// signer the session returns an `AuthError`, which real senders reject before they
    /// ever send a `LOAD` (OPEN-QUESTIONS Q2/Q11).
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<CastDeviceSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// The port senders should be pointed at (as advertised over mDNS).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.listen.port()
    }

    /// Serve one accepted connection to completion.
    async fn serve(&self, stream: TcpStream, peer: SocketAddr, sink: SessionSink) {
        // Nagle would sit on the small control frames this protocol is made of.
        if let Err(e) = stream.set_nodelay(true) {
            debug!(%peer, error = %e, "could not disable Nagle");
        }
        let mut tls = match self.acceptor.accept(stream).await {
            Ok(tls) => tls,
            Err(e) => {
                warn!(%peer, error = %e, "CASTv2 TLS handshake failed");
                return;
            }
        };
        info!(%peer, "CASTv2 sender connected");

        let auth = self.signer.clone().map(|signer| {
            Box::new(CastAuthResponder::new(signer, self.tls_cert_der.clone()))
                as Box<dyn crate::session::DeviceAuthResponder>
        });
        let mut session = CastSession::new(auth);

        if let Err(e) = self.pump(&mut tls, &mut session, &sink, peer).await {
            warn!(%peer, error = %e, "CASTv2 connection ended with an error");
        }
        // A dropped connection is a finished session, however it ended: tell the manager
        // so the pipeline doesn't hold the screen for a sender that walked away.
        let _ = sink.emit(SessionEvent::End).await;
        let _ = tls.shutdown().await;
        info!(%peer, "CASTv2 sender disconnected");
    }

    /// Read frames until the peer closes, folding each through the session.
    async fn pump(
        &self,
        tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
        session: &mut CastSession,
        sink: &SessionSink,
        peer: SocketAddr,
    ) -> Result<(), CastError> {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            let n = tls
                .read(&mut chunk)
                .await
                .map_err(|e| CastError::Io(e.to_string()))?;
            if n == 0 {
                return Ok(()); // clean EOF
            }
            buf.extend_from_slice(&chunk[..n]);

            // A frame that claims more than MAX_FRAME is never going to arrive; drop the
            // connection rather than buffering toward OOM.
            if buf.len() > MAX_FRAME {
                return Err(CastError::Io(format!("frame exceeds {MAX_FRAME} bytes")));
            }

            while let Some((msg, consumed)) = framing::try_decode(&buf)? {
                buf.drain(..consumed);
                let reaction = session.handle(&msg)?;
                for out in &reaction.outgoing {
                    let bytes = framing::encode(out)?;
                    tls.write_all(&bytes)
                        .await
                        .map_err(|e| CastError::Io(e.to_string()))?;
                }
                if !reaction.outgoing.is_empty() {
                    tls.flush()
                        .await
                        .map_err(|e| CastError::Io(e.to_string()))?;
                }
                if let Some(event) = reaction.event {
                    let ended = matches!(event, SessionEvent::End);
                    sink.emit(event)
                        .await
                        .map_err(|e| CastError::Io(e.to_string()))?;
                    if ended {
                        return Ok(());
                    }
                }
                if let Some(config) = reaction.start_mirror {
                    // Negotiation succeeded, but the RTP receive loop that would turn
                    // this into frames is Q12 — say so once, loudly, instead of leaving
                    // the sender streaming into a socket nobody reads.
                    warn!(
                        %peer,
                        udp_port = config.udp_port,
                        video = config.video.is_some(),
                        audio = config.audio.is_some(),
                        "Cast mirroring negotiated, but the RTP receive loop is not implemented (Q12)"
                    );
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl SourceAdapter for CastReceiver {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Cast
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        vec![Advertisement::MdnsService {
            ty: CAST_SERVICE_TYPE.to_string(),
            instance: self.friendly_name.clone(),
            port: self.port(),
            txt: vec![
                ("id".to_string(), self.device_id.clone()),
                ("md".to_string(), "castaway".to_string()),
                ("fn".to_string(), self.friendly_name.clone()),
                // Capability bitmask and protocol version, as senders expect them:
                // 5 = video out + audio out, "05" = the CASTv2 generation we speak.
                ("ca".to_string(), "5".to_string()),
                ("ve".to_string(), "05".to_string()),
            ],
        }]
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        let listener = TcpListener::bind(self.listen)
            .await
            .map_err(|e| CoreError::Adapter(format!("binding CASTv2 on {}: {e}", self.listen)))?;
        info!(addr = %self.listen, "CASTv2 TLS listener ready");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                // One failed accept (fd limit, RST between accept and return) shouldn't
                // take the listener down; the next sender deserves a try.
                Err(e) => {
                    warn!(error = %e, "CASTv2 accept failed");
                    continue;
                }
            };
            let this = Arc::clone(&self);
            // Tag events with the peer so two senders are distinguishable sources.
            let conn_sink = sink.with_instance(peer.to_string());
            tokio::spawn(async move { this.serve(stream, peer, conn_sink).await });
        }
    }
}

/// The default listen address for [`CAST_PORT`] on all interfaces.
#[must_use]
pub fn default_listen_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], CAST_PORT))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn identity() -> TlsIdentity {
        TlsIdentity::self_signed(&["castaway.local".to_string()]).unwrap()
    }

    fn receiver(identity: &TlsIdentity) -> CastReceiver {
        CastReceiver::new(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            "Lab TV",
            "0f8c2e10",
            identity,
        )
        .unwrap()
    }

    #[test]
    fn self_signed_identity_yields_a_usable_server_config() {
        let id = identity();
        assert!(!id.cert_der().is_empty());
        assert!(id.server_config().is_ok());
    }

    #[test]
    fn advertisement_carries_the_listening_port_and_name() {
        let id = identity();
        let r = CastReceiver::new(
            SocketAddr::from(([0, 0, 0, 0], 8009)),
            "Lab TV",
            "0f8c2e10",
            &id,
        )
        .unwrap();
        let ads = r.advertisements();
        match &ads[0] {
            Advertisement::MdnsService {
                ty,
                instance,
                port,
                txt,
            } => {
                assert_eq!(ty, CAST_SERVICE_TYPE);
                assert_eq!(instance, "Lab TV");
                assert_eq!(*port, 8009);
                assert!(txt.contains(&("fn".to_string(), "Lab TV".to_string())));
                assert!(txt.contains(&("id".to_string(), "0f8c2e10".to_string())));
            }
            other => panic!("expected an mDNS advertisement, got {other:?}"),
        }
    }

    #[test]
    fn kind_is_cast() {
        let id = identity();
        assert_eq!(receiver(&id).kind(), ProtocolKind::Cast);
    }
}
