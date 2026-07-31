//! The CASTv2 socket actor (OPEN-QUESTIONS Q15): the thin async shell around the pure
//! [`CastSession`]. It owns the TLS listener and one task per sender connection, and
//! does exactly three things per frame — decode, hand to the session, write back what
//! the session says to write. No protocol decisions live here (ground rule 3).
//!
//! Senders reach us over TLS with a certificate they never validate: CASTv2
//! authenticates the *device*, not the transport. The binding between the two is the
//! device-auth handshake, whose signature covers this connection's TLS certificate — so
//! the acceptor and the responder are drawn together, per connection, from one
//! [`CastIdentity`], and never chosen separately.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use castaway_core::{
    Advertisement, CoreError, MediaPorts, ProtocolKind, SessionEvent, SessionSink, SourceAdapter,
};
use crypto_cast_auth::CastDeviceSigner;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::auth::CastAuthResponder;
use crate::control::{CastRemote, FromReceiver};
use crate::error::CastError;
use crate::replay::ReplayIdentity;
use crate::rtp_actor::MirrorSocket;
use crate::session::{CastSession, DeviceAuthResponder};
use crate::{framing, CAST_PORT, CAST_SERVICE_TYPE};

/// Cap on a single CASTv2 frame. Real messages are a few KiB; the length prefix is
/// attacker-controlled, so it gets a bound rather than a `Vec` that grows to whatever a
/// sender claims.
const MAX_FRAME: usize = 1 << 20;

/// How far in the future the TLS certificate's `notAfter` is set.
///
/// This is a protocol constraint, not a policy preference. A Cast sender does not treat
/// the receiver's TLS certificate as web PKI — it never builds a chain for it — but it
/// *does* repurpose the X.509 validity window as the expiry of the device-auth signature
/// that covers it, and rejects any peer certificate whose `notAfter` is more than four
/// days out (`kMaxSelfSignedCertLifetimeInDays`, openscreen
/// `cast/sender/channel/cast_auth_util.cc`, the same code Chrome runs). rcgen's default
/// window is 1975→4096, which fails that check before device auth is even considered —
/// so an official sender walks away from a receiver that is otherwise perfectly correct,
/// and nothing on either side says why. Two days leaves two days of headroom for a
/// sender whose clock runs ahead of ours.
const TLS_CERT_VALID_FOR: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// How far `notBefore` is backdated. Nothing bounds this — the four-day rule is on
/// `notAfter` alone — so it is set purely to tolerate a sender whose clock trails ours,
/// which would otherwise see a certificate that is not valid yet.
const TLS_CERT_BACKDATE: Duration = Duration::from_secs(24 * 60 * 60);

/// Reissue once this much of the window is left. A receiver on a wall panel runs for
/// months; a certificate that is correct at boot and silently expires on day two is a
/// worse failure than never having had one, because the panel goes on looking healthy.
const TLS_CERT_RENEW_WITH: Duration = Duration::from_secs(24 * 60 * 60);

/// The receiver's TLS identity: one long-lived key plus a deliberately short-lived
/// self-signed certificate, reissued as it ages.
///
/// Self-signed is correct here, not a shortcut — every Cast receiver ships one, and
/// senders don't build a chain to a trust root. What matters is that the same DER bytes
/// the sender sees are the bytes the device-auth signature covers, and that the window
/// in those bytes satisfies `TLS_CERT_VALID_FOR`.
///
/// The key is kept across reissues rather than regenerated, which is what makes rotation
/// cheap enough to do on the accept path: issuing a certificate is microseconds, and only
/// key generation is slow.
pub struct TlsIdentity {
    key: rcgen::KeyPair,
    key_der: PrivateKeyDer<'static>,
    subject_alt_names: Vec<String>,
    current: Mutex<Issued>,
}

/// The certificate in force, and when it stops being worth serving.
struct Issued {
    config: Arc<rustls::ServerConfig>,
    cert_der: Vec<u8>,
    renew_at: SystemTime,
}

impl TlsIdentity {
    /// Generate a fresh self-signed identity for `subject_alt_names`.
    ///
    /// # Errors
    /// [`CastError::Tls`] if certificate generation fails.
    pub fn self_signed(subject_alt_names: &[String]) -> Result<Self, CastError> {
        Self::self_signed_at(subject_alt_names, SystemTime::now())
    }

    /// [`TlsIdentity::self_signed`] with the clock supplied, so a test can assert on the
    /// validity window without depending on when it runs.
    ///
    /// # Errors
    /// [`CastError::Tls`] if certificate generation fails.
    pub fn self_signed_at(
        subject_alt_names: &[String],
        now: SystemTime,
    ) -> Result<Self, CastError> {
        let key = rcgen::KeyPair::generate().map_err(|e| CastError::Tls(e.to_string()))?;
        Self::from_key_at(&key.serialize_der(), subject_alt_names, now)
    }

    /// [`TlsIdentity::self_signed_at`] with the key supplied as PKCS#8 DER.
    ///
    /// Exists so the device-auth vectors can be byte-reproducible: certificate issuance
    /// is deterministic, so fixing the key and the clock fixes the certificate, and
    /// therefore fixes the signature the vectors are checked against.
    ///
    /// # Errors
    /// [`CastError::Tls`] if the key cannot be parsed or the certificate not issued.
    pub fn from_key_at(
        key_pkcs8_der: &[u8],
        subject_alt_names: &[String],
        now: SystemTime,
    ) -> Result<Self, CastError> {
        let key =
            rcgen::KeyPair::try_from(key_pkcs8_der).map_err(|e| CastError::Tls(e.to_string()))?;
        let key_der = PrivateKeyDer::try_from(key.serialize_der())
            .map_err(|e| CastError::Tls(e.to_string()))?;
        let issued = Self::issue(&key, &key_der, subject_alt_names, now)?;
        Ok(Self {
            key,
            key_der,
            subject_alt_names: subject_alt_names.to_vec(),
            current: Mutex::new(issued),
        })
    }

    /// The certificate and acceptor to serve a connection arriving at `now` with,
    /// reissuing first if the current one is close enough to expiry that a sender would
    /// start refusing it.
    ///
    /// Reissue failure is deliberately not fatal: an aging certificate still authenticates
    /// this connection, and dropping senders because renewal failed would trade a
    /// degrading fault for an immediate one.
    fn current_at(&self, now: SystemTime) -> (TlsAcceptor, Vec<u8>) {
        let mut current = match self.current.lock() {
            Ok(guard) => guard,
            // The only way this is poisoned is a panic while holding it, and everything
            // under the lock is infallible cloning. Recover rather than propagate.
            Err(poisoned) => poisoned.into_inner(),
        };
        if now >= current.renew_at {
            match Self::issue(&self.key, &self.key_der, &self.subject_alt_names, now) {
                Ok(fresh) => {
                    debug!("reissued the Cast TLS certificate");
                    *current = fresh;
                }
                Err(e) => warn!(error = %e, "could not reissue the Cast TLS certificate"),
            }
        }
        (
            TlsAcceptor::from(Arc::clone(&current.config)),
            current.cert_der.clone(),
        )
    }

    /// The certificate DER in force — what the device-auth response signs over.
    #[must_use]
    pub fn cert_der(&self) -> Vec<u8> {
        self.current_at(SystemTime::now()).1
    }

    fn issue(
        key: &rcgen::KeyPair,
        key_der: &PrivateKeyDer<'static>,
        subject_alt_names: &[String],
        now: SystemTime,
    ) -> Result<Issued, CastError> {
        let mut params = rcgen::CertificateParams::new(subject_alt_names.to_vec())
            .map_err(|e| CastError::Tls(e.to_string()))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "castaway");
        params.not_before = offset_date_time(now - TLS_CERT_BACKDATE)?;
        params.not_after = offset_date_time(now + TLS_CERT_VALID_FOR)?;
        let cert = params
            .self_signed(key)
            .map_err(|e| CastError::Tls(e.to_string()))?;

        // Name the provider rather than taking `ServerConfig::builder()`'s process-default
        // path: that one *panics* if no default is installed and the crate features are
        // ambiguous, and a library crate doesn't get to panic (ground rule 7).
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| CastError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            key_der.clone_key(),
        )
        .map_err(|e| CastError::Tls(e.to_string()))?;

        Ok(Issued {
            config: Arc::new(config),
            cert_der: cert.der().to_vec(),
            renew_at: now + TLS_CERT_VALID_FOR - TLS_CERT_RENEW_WITH,
        })
    }
}

/// How this receiver proves it is a Cast device.
///
/// One enum rather than a TLS identity plus an optional signer, because the two
/// are not independent: the device-auth signature covers the TLS certificate, and
/// a pairing that disagrees produces a receiver that completes its handshake and
/// then fails every challenge — with nothing on either side saying why. Making
/// the combinations enumerable means the only way to pick a certificate is to
/// pick the credential it belongs to.
pub enum CastIdentity {
    /// A self-signed certificate and no device credential. Device auth is refused,
    /// which every official sender treats as fatal before it sends a `LOAD`.
    /// Useful only against senders that do not challenge.
    Unauthenticated(TlsIdentity),

    /// A self-signed certificate signed over by a device key we hold.
    ///
    /// Correct by construction, and rejected by every official sender unless the
    /// chain roots in Google's device CA — which, for a locally generated
    /// credential, it does not (#40).
    DeviceKey {
        /// The TLS identity whose certificate the signature covers.
        tls: TlsIdentity,
        /// The key that signs it.
        signer: Arc<CastDeviceSigner>,
    },

    /// A CKS credential: a real Google-issued chain with a precomputed signature.
    ///
    /// The TLS certificate comes from the credential, so it is not named here
    /// separately — there is nothing to keep in sync.
    Replay(Arc<ReplayIdentity>),
}

impl CastIdentity {
    /// Self-signed TLS with a device key of our own.
    #[must_use]
    pub fn device_key(tls: TlsIdentity, signer: Arc<CastDeviceSigner>) -> Self {
        Self::DeviceKey { tls, signer }
    }

    /// A CKS-provisioned credential.
    #[must_use]
    pub fn replay(provider: Arc<cast_replay::ReplayProvider>) -> Self {
        Self::Replay(Arc::new(ReplayIdentity::new(provider)))
    }

    /// The acceptor to serve a connection arriving at `now` with, and the responder
    /// that answers its challenge.
    ///
    /// Returned together so a caller cannot take one from one credential and one
    /// from another. `None` for the responder means device auth is refused.
    fn for_connection(
        &self,
        now: SystemTime,
    ) -> Result<(TlsAcceptor, Option<Box<dyn DeviceAuthResponder>>), CastError> {
        match self {
            Self::Unauthenticated(tls) => {
                let (acceptor, _cert) = tls.current_at(now);
                Ok((acceptor, None))
            }
            Self::DeviceKey { tls, signer } => {
                let (acceptor, cert_der) = tls.current_at(now);
                Ok((
                    acceptor,
                    Some(
                        Box::new(CastAuthResponder::new(Arc::clone(signer), cert_der))
                            as Box<dyn DeviceAuthResponder>,
                    ),
                ))
            }
            Self::Replay(cks) => {
                let (acceptor, responder) = cks.for_connection()?;
                Ok((acceptor, Some(responder)))
            }
        }
    }

    /// A one-line description for the startup log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unauthenticated(_) => "no device credential; challenges will be refused".into(),
            Self::DeviceKey { .. } => {
                "a self-generated device key; senders that verify the Google chain will reject it"
                    .into()
            }
            Self::Replay(cks) => {
                let credential = cks.credential();
                format!(
                    "a CKS credential from the {}, valid until {}",
                    credential.origin(),
                    credential.window().end_unix()
                )
            }
        }
    }
}

/// `SystemTime` in the shape rcgen wants.
fn offset_date_time(at: SystemTime) -> Result<OffsetDateTime, CastError> {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| CastError::Tls(e.to_string()))?
        .as_secs();
    let secs = i64::try_from(secs).map_err(|e| CastError::Tls(e.to_string()))?;
    OffsetDateTime::from_unix_timestamp(secs).map_err(|e| CastError::Tls(e.to_string()))
}

/// A listening CASTv2 receiver: one TLS listener, one [`CastSession`] per connection.
pub struct CastReceiver {
    listen: SocketAddr,
    friendly_name: String,
    device_id: String,
    identity: CastIdentity,
    /// Where each session's mirroring RTP socket binds (the port an ANSWER names).
    media_ports: MediaPorts,
    /// Where playback has reached, for the `currentTime` a sender draws its scrubber from.
    ///
    /// Absent in a build with no decoder, which then reports zero — the wire has no slot
    /// for "unknown", so the honest thing available is the value a sender sees for the
    /// length of a fetch anyway.
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
}

impl CastReceiver {
    /// Build a receiver listening on `listen` with the given `identity`.
    ///
    /// The identity is owned rather than borrowed because it is not a fixed value: a
    /// self-signed certificate is short-lived by protocol requirement (see
    /// `TLS_CERT_VALID_FOR`) and is reissued as connections arrive, and a replayed
    /// credential rolls with its window.
    ///
    /// # Errors
    /// Currently infallible; kept fallible because construction reaches TLS setup.
    pub fn new(
        listen: SocketAddr,
        friendly_name: impl Into<String>,
        device_id: impl Into<String>,
        identity: CastIdentity,
        media_ports: MediaPorts,
    ) -> Result<Self, CastError> {
        Ok(Self {
            listen,
            friendly_name: friendly_name.into(),
            device_id: device_id.into(),
            identity,
            media_ports,
            playback: None,
        })
    }

    /// Let this receiver ask the pipeline where playback has reached.
    ///
    /// Cast is the second protocol in which the receiver *is* the player, so a sender's
    /// scrubber can only be answered from here. Without it `currentTime` is zero for the
    /// whole item — which is what it was, knowingly, until the pipeline had a position to
    /// report at all.
    #[must_use]
    pub fn with_playback(mut self, report: Arc<dyn castaway_core::PlaybackReport>) -> Self {
        self.playback = Some(report);
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
        // Taken per connection, not once at startup: the credential rotates — a
        // self-signed certificate is reissued as it ages, a CKS one rolls with its
        // window — and the device-auth signature has to cover the certificate this
        // sender is actually looking at. Both come back from one call so they cannot
        // be drawn from different credentials.
        let (acceptor, auth) = match self.identity.for_connection(SystemTime::now()) {
            Ok(pair) => pair,
            Err(e) => {
                warn!(%peer, error = %e, "no usable Cast identity for this connection");
                return;
            }
        };
        let mut tls = match acceptor.accept(stream).await {
            Ok(tls) => tls,
            Err(e) => {
                warn!(%peer, error = %e, "CASTv2 TLS handshake failed");
                return;
            }
        };
        info!(%peer, "CASTv2 sender connected");

        let mut session = CastSession::new(auth);

        // Bind the RTP socket up front. The ANSWER has to name a port, and the only way
        // to name one we are certain of is to already hold it. A failure here costs
        // mirroring, not the connection — the media-URL path does not need a socket.
        let mut rtp = match MirrorSocket::bind(self.listen.ip(), self.media_ports).await {
            Ok(socket) => {
                session = session.with_mirror_port(socket.port());
                Some(socket)
            }
            Err(e) => {
                warn!(%peer, error = %e, "no RTP socket; mirroring will be declined");
                None
            }
        };

        // The reverse channel. A Cast sender has no way to learn what became of the URL it
        // handed over except by being told, and the only thing that knows is the pipeline —
        // which reaches this connection through here. Publishing it also gives the panel a
        // Cast session it can drive, on the same terms it drives a DLNA one.
        //
        // Built here, *published* only after a `Play` (see `with_control_surface`): the
        // session manager only accepts a control surface from the source that holds the
        // screen, so emitting it at connection time — before any LOAD — was a guaranteed
        // drop. Every sender opens idle status connections long before it casts, and each
        // one used to log `no active session` here while the panel never got a remote it
        // could drive and `media_ended` never reached the sender (its queue simply stuck).
        let (to_actor, from_receiver) = tokio::sync::mpsc::channel(8);
        let remote = Arc::new(CastRemote::new(to_actor, sink.clone()));

        let mut mirror_task: Option<JoinHandle<()>> = None;
        if let Err(e) = self
            .pump(
                &mut tls,
                &mut session,
                &sink,
                peer,
                &mut rtp,
                &mut mirror_task,
                from_receiver,
                &remote,
            )
            .await
        {
            warn!(%peer, error = %e, "CASTv2 connection ended with an error");
        }
        // The control connection is what authorizes the media stream. When it goes, the
        // RTP loop goes with it, whatever the pipeline still holds.
        if let Some(task) = mirror_task {
            task.abort();
        }
        // A dropped connection is a finished session, however it ended: tell the manager
        // so the pipeline doesn't hold the screen for a sender that walked away.
        let _ = sink.emit(SessionEvent::End).await;
        let _ = tls.shutdown().await;
        info!(%peer, "CASTv2 sender disconnected");
    }

    /// Read frames until the peer closes, folding each through the session.
    #[allow(clippy::too_many_arguments)]
    async fn pump(
        &self,
        tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
        session: &mut CastSession,
        sink: &SessionSink,
        peer: SocketAddr,
        rtp: &mut Option<MirrorSocket>,
        mirror_task: &mut Option<JoinHandle<()>>,
        mut from_receiver: tokio::sync::mpsc::Receiver<FromReceiver>,
        remote: &Arc<CastRemote>,
    ) -> Result<(), CastError> {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            // Two sources, one owner. The session is folded from here and nowhere else, so
            // an unsolicited status and a reply to a sender cannot interleave halfway
            // through a frame — which is what a shared session behind a mutex would have
            // to be careful about and this does not.
            let n = tokio::select! {
                read = tls.read(&mut chunk) => read.map_err(|e| CastError::Io(e.to_string()))?,
                Some(request) = from_receiver.recv() => {
                    let outgoing = match request {
                        FromReceiver::Ended(end) => {
                            info!(%peer, %end, "cast: the pipeline finished with the item");
                            session.media_ended(&end)
                        }
                        FromReceiver::Control(txn) => session.apply_local_control(&txn),
                    };
                    Self::write_all(tls, &outgoing).await?;
                    continue;
                }
            };
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
                // The only view of sender traffic on a box nobody can attach a
                // debugger to. Payloads are logged whole: a session that dies on one
                // message can only be diagnosed by seeing that message.
                tracing::trace!(
                    %peer,
                    namespace = %msg.namespace,
                    source = %msg.source_id,
                    destination = %msg.destination_id,
                    payload = %msg.payload_utf8.as_deref().unwrap_or("<binary>"),
                    "cast message in"
                );
                // Where playback has reached, handed to the session as an input so it stays
                // a pure fold — the same shape `proto-dlna` uses, and what keeps every
                // status test free of a live decoder.
                session.observe_progress(self.playback.as_ref().and_then(|p| p.progress()));
                let reaction = session.handle(&msg)?;
                Self::write_all(tls, &reaction.outgoing).await?;
                for event in with_control_surface(reaction.events, remote) {
                    let ended = matches!(event, SessionEvent::End);
                    sink.emit(event)
                        .await
                        .map_err(|e| CastError::Io(e.to_string()))?;
                    if ended {
                        // The session is over; the channel is not. This socket is
                        // shared transport for every sender on this peer, and a STOP
                        // is routinely followed on the same socket by the sender's
                        // next move — Chrome's YouTube handoff sends STOP and then
                        // LAUNCH back-to-back. Hanging up here snapped that LAUNCH
                        // mid-flight and turned every handoff into "Failed to cast".
                        if let Some(task) = mirror_task.take() {
                            task.abort();
                        }
                        if rtp.is_none() {
                            // The old socket was consumed by the mirror that just
                            // ended; re-arm so a fresh OFFER on this connection gets
                            // a live port in its ANSWER rather than the dead one.
                            match MirrorSocket::bind(self.listen.ip(), self.media_ports).await {
                                Ok(socket) => {
                                    session.set_mirror_port(Some(socket.port()));
                                    *rtp = Some(socket);
                                }
                                Err(e) => {
                                    warn!(%peer, error = %e, "could not re-arm mirroring after session end");
                                    session.set_mirror_port(None);
                                }
                            }
                        }
                    }
                }
                if let Some(config) = reaction.start_mirror {
                    // The socket is consumed here, so a second OFFER on one connection
                    // finds nothing to bind and is ignored. Senders renegotiate by
                    // reconnecting, which gets a fresh socket with it.
                    let Some(socket) = rtp.take() else {
                        warn!(%peer, "mirroring already started on this connection");
                        continue;
                    };
                    info!(
                        %peer,
                        udp_port = config.udp_port,
                        video = ?config.video.codec,
                        audio = ?config.audio.as_ref().map(|a| a.codec),
                        "Cast mirroring negotiated"
                    );
                    let (video, audio, receive_loop) = socket.start(&config);
                    *mirror_task = Some(tokio::spawn(receive_loop.run()));
                    // Cast mirror audio is Opus at 48 kHz stereo, which is self-describing
                    // in-band — so no codec configuration, unlike AirPlay's AAC-ELD. Until
                    // now this source was handed over as a bare `FrameSource` and the render
                    // pipeline discarded it without a word.
                    let audio = audio.and_then(|source| {
                        castaway_core::AudioFormat::from_hz(48_000, 2).map(|format| {
                            castaway_core::MirrorAudio {
                                source,
                                format,
                                config: None,
                            }
                        })
                    });
                    sink.emit(SessionEvent::Mirror { video, audio })
                        .await
                        .map_err(|e| CastError::Io(e.to_string()))?;
                }
            }
        }
    }

    /// Frame and write a batch of messages, flushing once at the end.
    async fn write_all(
        tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
        outgoing: &[crate::proto::CastMessage],
    ) -> Result<(), CastError> {
        if outgoing.is_empty() {
            return Ok(());
        }
        for out in outgoing {
            let bytes = framing::encode(out)?;
            tls.write_all(&bytes)
                .await
                .map_err(|e| CastError::Io(e.to_string()))?;
        }
        tls.flush().await.map_err(|e| CastError::Io(e.to_string()))
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
                // Receiver status flag, and it is *mandatory* — openscreen's
                // `ReceiverInfoFromDnsSdInstance` rejects the whole record with
                // "Missing receiver status flag" when `st` is absent, so a sender that
                // parses records strictly drops this device before opening a socket. A
                // discovery failure and a protocol failure look identical from the room:
                // the panel is simply not in the list.
                //
                // 0 is idle, 1 is busy. This is fixed at idle because the advertisement
                // is built once at startup; a receiver that is playing still says idle,
                // which costs a sender the "someone is already casting" hint and nothing
                // else. Flipping it live needs re-advertisement on every session change.
                ("st".to_string(), "0".to_string()),
            ],
        }]
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "registered: the cast/tcp 8009 entry in crates/app/src/surface.rs"
        )]
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

/// Splice this connection's control surface into a reaction's event stream, immediately
/// after the event that makes the connection the active source.
///
/// The session manager only accepts a `ControlSurface` from the source that currently
/// holds the screen, so the surface must *follow* `Play` — published at connection time
/// (as it used to be) it was dropped every single time, the panel got no remote to
/// drive, and the pipeline's end-of-media report never reached the sender, whose queue
/// simply stuck. Re-published after every `Play` rather than once per connection: VLC
/// sends STOP + LOAD for every scrub, and each fresh hold on the screen needs the
/// surface again. Mirroring deliberately gets none — there is no media session behind
/// it to drive, so a transport strip would be all buttons and no effect.
fn with_control_surface(events: Vec<SessionEvent>, remote: &Arc<CastRemote>) -> Vec<SessionEvent> {
    let mut out = Vec::with_capacity(events.len() + 1);
    for event in events {
        let begins = matches!(event, SessionEvent::Play { .. });
        out.push(event);
        if begins {
            out.push(SessionEvent::ControlSurface(remote.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn identity() -> TlsIdentity {
        TlsIdentity::self_signed(&["castaway.local".to_string()]).unwrap()
    }

    fn receiver(identity: TlsIdentity) -> CastReceiver {
        CastReceiver::new(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            "Lab TV",
            "0f8c2e10",
            CastIdentity::Unauthenticated(identity),
            MediaPorts::Ephemeral,
        )
        .unwrap()
    }

    #[test]
    fn self_signed_identity_yields_a_usable_server_config() {
        let id = identity();
        assert!(!id.cert_der().is_empty());
        assert!(!id.current_at(SystemTime::now()).1.is_empty());
    }

    /// The window is the whole point of issuing our own certificate rather than taking
    /// rcgen's default one: a sender rejects a peer certificate valid for longer than
    /// four days, and rcgen's default runs to the year 4096.
    #[test]
    fn tls_certificate_expires_inside_the_senders_four_day_limit() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let id = TlsIdentity::self_signed_at(&["castaway.local".to_string()], now).unwrap();
        let der = id.current_at(now).1;
        let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();

        let not_after = cert.validity().not_after.timestamp();
        let not_before = cert.validity().not_before.timestamp();
        let now_secs = 1_800_000_000i64;

        assert!(
            not_after - now_secs < 4 * 24 * 60 * 60,
            "notAfter is {} days out; a sender caps this at four",
            (not_after - now_secs) / 86_400
        );
        assert!(not_after > now_secs, "the certificate must not be expired");
        assert!(
            not_before < now_secs,
            "notBefore must be in the past, or a sender whose clock trails ours sees a \
             certificate that is not yet valid"
        );
    }

    /// A receiver that runs past its certificate's life must not keep serving the expired
    /// one: senders would refuse a panel that had been working for two days, with nothing
    /// on either side saying why.
    #[test]
    fn the_certificate_is_reissued_before_it_expires() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let id = TlsIdentity::self_signed_at(&["castaway.local".to_string()], now).unwrap();
        let first = id.current_at(now).1;
        assert_eq!(
            first,
            id.current_at(now).1,
            "not reissued while still fresh"
        );

        let later = now + TLS_CERT_VALID_FOR;
        let second = id.current_at(later).1;
        assert_ne!(first, second, "the aging certificate was served again");

        let (_, cert) = x509_parser::parse_x509_certificate(&second).unwrap();
        let later_secs = 1_800_000_000i64 + i64::try_from(TLS_CERT_VALID_FOR.as_secs()).unwrap();
        assert!(cert.validity().not_after.timestamp() > later_secs);
    }

    #[test]
    fn advertisement_carries_the_listening_port_and_name() {
        let r = CastReceiver::new(
            SocketAddr::from(([0, 0, 0, 0], 8009)),
            "Lab TV",
            "0f8c2e10",
            CastIdentity::Unauthenticated(identity()),
            MediaPorts::Ephemeral,
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
                // Every key openscreen's record parser treats as mandatory. Miss one
                // and a strict sender discards the advertisement entirely, which in
                // the room is indistinguishable from the receiver being switched off.
                for key in ["id", "ve", "ca", "st", "fn"] {
                    assert!(
                        txt.iter().any(|(k, _)| k == key),
                        "the {key} TXT key is required; without it the record is dropped"
                    );
                }
            }
            other => panic!("expected an mDNS advertisement, got {other:?}"),
        }
    }

    #[test]
    fn kind_is_cast() {
        assert_eq!(receiver(identity()).kind(), ProtocolKind::Cast);
    }

    /// The manager only accepts a control surface from the active source, so the
    /// surface has to come after the `Play` that makes this connection active —
    /// and must not be offered by connections that never play (every sender opens
    /// idle status connections, which used to spam `no active session` drops).
    #[test]
    fn the_control_surface_follows_play_and_never_precedes_it() {
        let (to_actor, _actor_rx) = tokio::sync::mpsc::channel(1);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let sink = SessionSink::new(castaway_core::SourceId::new(ProtocolKind::Cast, "test"), tx);
        let remote = Arc::new(CastRemote::new(to_actor, sink));

        // A status-only connection produces no session event → no surface.
        assert!(with_control_surface(vec![], &remote).is_empty());

        // A LOAD's Play is immediately followed by the surface, in that order.
        let play = SessionEvent::Play {
            source: castaway_core::MediaUri::parse("http://10.0.0.2/film.mkv").unwrap(),
            start: None,
        };
        let out = with_control_surface(vec![play], &remote);
        assert!(matches!(out[0], SessionEvent::Play { .. }));
        assert!(matches!(out[1], SessionEvent::ControlSurface(_)));
        assert_eq!(out.len(), 2);

        // Events that do not take the screen get no surface attached.
        let out = with_control_surface(vec![SessionEvent::End], &remote);
        assert!(matches!(out[..], [SessionEvent::End]));
    }
}
