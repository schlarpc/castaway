//! The [`SourceAdapter`]: one async actor that owns the transport and composes every
//! layer beneath it.
//!
//! Everything below this file is pure. This is the only place a socket exists, and its
//! whole job is to move bytes between the transport and the state machines — HCI events
//! to [`HostController`], ACL fragments through [`Reassembler`] to [`Multiplexer`], and
//! L2CAP channel data to whichever of SDP, AVDTP or AVRCP owns that PSM (ground rule 3).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use castaway_core::{
    Advertisement, AudioFormat, CoreError, EncodedFrame, FrameSource, NowPlaying, ProtocolKind,
    SessionEvent, SessionSink, SourceAdapter, SourceDescription,
};
use substrate_hci::{
    BdAddr, ConnectionHandle, Event, HciPacket, HciTransport, LinkKey, Reassembler,
};
use substrate_l2cap::{Cid, L2capEvent, L2capPdu, Multiplexer, Psm};
use substrate_sdp::{a2dp_sink, avrcp_controller, avrcp_target, SdpServer};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::acl::AclWriter;
use crate::avctp::{AvcFrame, AvctpMessage, Ctype};
use crate::avrcp;
use crate::codec::advertised;
use crate::control::AvrcpControl;
use crate::host::{HostAction, HostConfig, HostController};
use crate::media::Depacketizer;
use crate::sink::{SinkEvent, SinkSession};
use crate::{avdtp, Message};

/// How many encoded frames may queue before the oldest are dropped.
///
/// Audio, unlike video, must not drop frames casually — a gap is audible where a dropped
/// video frame is not. The buffer is sized generously and a full one is logged rather
/// than silently absorbed, because it means decode is not keeping up.
const AUDIO_QUEUE_DEPTH: usize = 256;

/// Called when a phone pairs, so the caller can persist its link key.
///
/// A callback rather than a path, because this crate must not open files (ground rule
/// 2): where the config directory lives is the app's business, and keeping it out of
/// here is what lets the whole adapter be tested with no filesystem at all.
pub type OnPaired = Arc<dyn Fn(BdAddr, LinkKey) + Send + Sync>;

/// Configuration for the Bluetooth adapter.
#[derive(Clone)]
pub struct BluetoothConfig {
    /// Controller bring-up settings.
    pub host: HostConfig,
    /// Whether to advertise the LDAC endpoint. Should mirror the `ldac` build feature —
    /// advertising a codec we cannot decode makes the session silence (Q22).
    pub enable_ldac: bool,
    /// Restrict the advertised endpoints to these codecs. `None` advertises everything
    /// the build supports, which is what a deployment wants.
    ///
    /// Exists for bring-up: a sender picks the first endpoint it also supports, so the
    /// only way to exercise a *particular* codec against real hardware is to stop
    /// offering the ones it would otherwise prefer. Narrowing this to SBC is how the
    /// mandatory fallback path gets tested at all.
    pub codecs: Option<Vec<castaway_core::AudioCodec>>,
    /// Link keys loaded from disk, so repeat guests reconnect silently (Q23).
    pub link_keys: Vec<(BdAddr, LinkKey)>,
    /// Called with each newly paired peer's key. Without one, pairing works for the
    /// current session and every guest re-pairs after a restart.
    pub on_paired: Option<OnPaired>,
}

impl std::fmt::Debug for BluetoothConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BluetoothConfig")
            .field("host", &self.host)
            .field("enable_ldac", &self.enable_ldac)
            .field("codecs", &self.codecs)
            .field("link_keys", &self.link_keys.len())
            .field("persists_keys", &self.on_paired.is_some())
            .finish()
    }
}

// Not derivable, despite appearances: `enable_ldac` follows the build feature, and
// `bool::default()` is `false`. Deriving would compile and quietly disable LDAC in
// exactly the builds that went to the trouble of enabling it.
#[allow(clippy::derivable_impls)]
impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            host: HostConfig::default(),
            enable_ldac: cfg!(feature = "ldac"),
            codecs: None,
            link_keys: Vec::new(),
            on_paired: None,
        }
    }
}

/// Per-ACL-link state.
struct Link {
    peer: BdAddr,
    reassembler: Reassembler,
    mux: Multiplexer,
    sink: SinkSession,
    /// AVDTP opens *two* channels on the same PSM: signaling first, then a separate
    /// media transport channel. They are told apart by arrival order, which is the only
    /// signal the protocol gives — and mixing them up feeds audio to the signaling
    /// parser and produces a stream of "unknown signal" rejects.
    avdtp_signaling: Option<Cid>,
    avdtp_media: Option<Cid>,
    avctp: Option<Cid>,
    depacketizer: Option<Depacketizer>,
    /// What AVDTP negotiated, held from SET_CONFIGURATION until START. aptX carries no
    /// in-band rate, so this is the decoder's only source of it (OPEN-QUESTIONS Q25).
    audio_format: Option<AudioFormat>,
    audio_tx: Option<mpsc::Sender<EncodedFrame>>,
    /// Whether a `SessionEvent::Audio` has already been emitted for this link.
    session_open: bool,
    /// Metadata accumulated for this link, re-emitted as a full snapshot on change.
    now_playing: NowPlaying,
    /// Next AVCTP transaction label.
    avctp_transaction: u8,
    /// What we know about this phone: address from link-up, name from the remote-name
    /// request, codec from AVDTP configuration. Each arrives separately.
    description: SourceDescription,
}

impl Link {
    fn new(peer: BdAddr, capabilities: Vec<crate::codec::CodecCapability>) -> Self {
        let mut mux = Multiplexer::new(672);
        mux.listen(Psm::SDP);
        mux.listen(Psm::AVDTP);
        mux.listen(Psm::AVCTP);
        Self {
            peer,
            reassembler: Reassembler::new(),
            mux,
            sink: SinkSession::new(capabilities),
            avdtp_signaling: None,
            avdtp_media: None,
            avctp: None,
            depacketizer: None,
            audio_format: None,
            audio_tx: None,
            session_open: false,
            now_playing: NowPlaying::default(),
            avctp_transaction: 0,
            description: SourceDescription::new().with_address(peer.to_string()),
        }
    }

    fn next_transaction(&mut self) -> u8 {
        let t = self.avctp_transaction;
        self.avctp_transaction = (self.avctp_transaction + 1) & 0x0F;
        t
    }
}

/// The Bluetooth A2DP sink adapter.
pub struct BluetoothAdapter {
    transport: Arc<dyn HciTransport>,
    config: BluetoothConfig,
    sdp: SdpServer,
    /// The endpoint table every link advertises, resolved once.
    capabilities: Vec<crate::codec::CodecCapability>,
}

impl std::fmt::Debug for BluetoothAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BluetoothAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl BluetoothAdapter {
    /// Build an adapter over a transport.
    #[must_use]
    pub fn new(transport: Arc<dyn HciTransport>, config: BluetoothConfig) -> Self {
        let name = config.host.name.clone();
        let mut sdp = SdpServer::new();
        sdp.add(a2dp_sink(0x0001_0000, &name));
        // Both AVRCP records: Controller so we can drive the phone's player, Target so
        // its volume rocker reaches us (Q24). Publishing one loses half the feature.
        sdp.add(avrcp_controller(0x0001_0001, &name));
        sdp.add(avrcp_target(0x0001_0002, &name));
        let mut capabilities = advertised(config.enable_ldac);
        if let Some(allowed) = &config.codecs {
            capabilities.retain(|c| allowed.contains(&c.audio_codec()));
        }
        Self {
            transport,
            config,
            sdp,
            capabilities,
        }
    }

    /// The codecs this adapter advertises, in preference order.
    #[must_use]
    pub fn advertised_codecs(&self) -> Vec<castaway_core::AudioCodec> {
        self.capabilities
            .iter()
            .map(crate::codec::CodecCapability::audio_codec)
            .collect()
    }

    /// Send one HCI packet.
    async fn send(&self, packet: HciPacket) -> Result<(), CoreError> {
        self.transport
            .send(packet)
            .await
            .map_err(|e| CoreError::Adapter(e.to_string()))
    }
}

#[async_trait::async_trait]
impl SourceAdapter for BluetoothAdapter {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Bluetooth
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // Bluetooth is its own discovery layer: inquiry scan and the SDP records, not
        // mDNS or SSDP. There is nothing for the shared responders to publish.
        Vec::new()
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        let mut host = HostController::new(self.config.host.clone());
        host.load_link_keys(self.config.link_keys.iter().copied());

        for action in host.start() {
            self.apply_host_action(&action, &mut host).await?;
        }

        let mut links: HashMap<u16, Link> = HashMap::new();
        // Every outbound PDU goes through here: one writer, paced by the controller's
        // buffer credits, so nothing is written into a buffer that does not exist and no
        // two PDUs interleave their fragments (Q26).
        let acl = AclWriter::spawn(Arc::clone(&self.transport));

        loop {
            let packet = match self.transport.recv().await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "bluetooth transport ended");
                    return Ok(());
                }
            };

            match packet {
                HciPacket::Event { code, params } => {
                    let event = match Event::parse(code, &params) {
                        Ok(ev) => ev,
                        Err(e) => {
                            debug!(error = %e, code, "undecodable HCI event");
                            continue;
                        }
                    };
                    debug!(?event, "hci event");
                    for action in host.on_event(&event) {
                        match &action {
                            HostAction::Ready {
                                address,
                                acl_credits,
                                acl_mtu,
                            } => {
                                acl.configure(*acl_credits, *acl_mtu).await;
                                info!(
                                    %address,
                                    acl_credits,
                                    acl_mtu,
                                    "bluetooth: discoverable"
                                );
                            }
                            HostAction::Credits { handle, count } => {
                                acl.completed(*handle, *count).await;
                            }
                            HostAction::LinkUp { handle, peer } => {
                                info!(%peer, "bluetooth: link up");
                                links.insert(
                                    handle.raw(),
                                    Link::new(*peer, self.capabilities.clone()),
                                );
                            }
                            HostAction::PeerName { peer, name } => {
                                if let Some(link) = links.values_mut().find(|l| l.peer == *peer) {
                                    link.description = std::mem::take(&mut link.description)
                                        .merged(SourceDescription::new().with_display_name(name));
                                    if link.session_open {
                                        let link_sink = sink.with_instance(peer.to_string());
                                        link_sink
                                            .emit(SessionEvent::SourceInfo(
                                                link.description.clone(),
                                            ))
                                            .await?;
                                    }
                                }
                            }
                            HostAction::LinkDown {
                                handle,
                                peer,
                                reason,
                            } => {
                                // The reason separates "authentication failed" from
                                // "connection timeout" from "the phone walked away".
                                // Without it every failure reads the same.
                                info!(%peer, %reason, "bluetooth: link down");
                                // The controller flushed whatever was queued for this
                                // handle without ever reporting it complete, so the
                                // credits have to be taken back by hand.
                                acl.link_down(*handle).await;
                                if let Some(mut link) = links.remove(&handle.raw()) {
                                    // Reap the whole session: the phone left without a
                                    // teardown handshake, which is the ordinary case.
                                    let _ = link.sink.link_down();
                                    let _ = link.mux.link_down();
                                    if link.session_open {
                                        let link_sink = sink.with_instance(link.peer.to_string());
                                        link_sink.emit(SessionEvent::End).await?;
                                    }
                                }
                            }
                            _ => {}
                        }
                        self.apply_host_action(&action, &mut host).await?;
                    }
                }

                HciPacket::Acl(packet) => {
                    let packet_handle = packet.handle;
                    let Some(link) = links.get_mut(&packet_handle.raw()) else {
                        debug!(handle = %packet_handle, "ACL for an unknown link");
                        continue;
                    };
                    let pdu_bytes = match link.reassembler.push(&packet) {
                        Ok(Some(bytes)) => bytes,
                        Ok(None) => continue,
                        Err(e) => {
                            warn!(error = %e, "ACL reassembly failed");
                            continue;
                        }
                    };
                    let pdu = match L2capPdu::decode(&pdu_bytes) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "malformed L2CAP PDU");
                            continue;
                        }
                    };
                    let events = match link.mux.handle_pdu(&pdu) {
                        Ok(evs) => evs,
                        Err(e) => {
                            debug!(error = %e, "L2CAP rejected a PDU");
                            continue;
                        }
                    };
                    self.dispatch(
                        packet_handle,
                        events,
                        links.get_mut(&packet_handle.raw()),
                        &sink,
                        &acl,
                    )
                    .await?;
                }

                other => debug!(?other, "ignoring HCI packet"),
            }
        }
    }
}

impl BluetoothAdapter {
    async fn apply_host_action(
        &self,
        action: &HostAction,
        _host: &mut HostController,
    ) -> Result<(), CoreError> {
        if let HostAction::Send(command) = action {
            let packet = command
                .encode()
                .map_err(|e| CoreError::Adapter(format!("hci encode: {e}")))?;
            self.send(packet).await?;
        }
        if let HostAction::Paired { peer, key } = action {
            info!(%peer, "bluetooth: paired");
            // Persistence is the app's job — it owns the config directory — so the key
            // goes out through a callback rather than to a path this crate knows.
            if let Some(on_paired) = &self.config.on_paired {
                on_paired(*peer, *key);
            }
        }
        Ok(())
    }

    /// Route L2CAP events for one link.
    async fn dispatch(
        &self,
        handle: ConnectionHandle,
        events: Vec<L2capEvent>,
        link: Option<&mut Link>,
        sink: &SessionSink,
        acl: &AclWriter,
    ) -> Result<(), CoreError> {
        let Some(link) = link else {
            return Ok(());
        };
        // Signalling PDUs the multiplexer built are already addressed to the peer.
        let mut signalling: Vec<L2capPdu> = Vec::new();
        // Protocol replies are keyed by *our* channel id — the one the incoming event
        // named — and the multiplexer maps each to the peer's before it goes out.
        let mut outbound: Vec<(Cid, Bytes)> = Vec::new();

        for event in events {
            match event {
                L2capEvent::Send(pdu) => signalling.push(pdu),

                L2capEvent::ChannelOpen { cid, psm, .. } => {
                    if psm == Psm::AVDTP {
                        // Order is the only discriminator the protocol offers.
                        if link.avdtp_signaling.is_none() {
                            link.avdtp_signaling = Some(cid);
                        } else {
                            link.avdtp_media = Some(cid);
                        }
                    } else if psm == Psm::AVCTP {
                        link.avctp = Some(cid);
                        // The control channel is up, so the receiver can now drive the
                        // sender. This is deliberately its own event rather than part of
                        // session start — AVCTP routinely connects after audio is
                        // already flowing.
                        let (tx, rx) = mpsc::channel(32);
                        let control = Arc::new(AvrcpControl::passthrough(tx));
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::ControlSurface(control))
                            .await?;
                        // Resolve the peer's identifier once, here: the writer task
                        // outlives this borrow and cannot consult the multiplexer later.
                        let peer_cid = link.mux.channel(cid).map(|c| c.remote_cid);
                        if let Some(peer_cid) = peer_cid {
                            Self::spawn_control_writer(handle, peer_cid, rx, acl.clone());
                        } else {
                            warn!(%cid, "avctp channel vanished before its writer started");
                        }
                        // Ask for metadata straight away rather than waiting for a
                        // notification; a track already playing produces no change event.
                        let transaction = link.next_transaction();
                        outbound.push((
                            cid,
                            avctp_body(
                                transaction,
                                &avrcp::get_element_attributes(&avrcp::attribute::ALL),
                            ),
                        ));
                    }
                    debug!(%cid, %psm, "l2cap channel open");
                }

                L2capEvent::ChannelClosed { cid, psm } => {
                    if Some(cid) == link.avdtp_media {
                        link.avdtp_media = None;
                        link.audio_tx = None;
                    } else if Some(cid) == link.avdtp_signaling {
                        link.avdtp_signaling = None;
                    } else if Some(cid) == link.avctp {
                        link.avctp = None;
                    }
                    debug!(%cid, %psm, "l2cap channel closed");
                }

                L2capEvent::Data { cid, psm, payload } => {
                    if psm == Psm::SDP {
                        let response = self.sdp.handle(&payload);
                        // Both sides in full: an SDP exchange that a peer walks away from
                        // cannot be diagnosed from our side's opinion of it.
                        debug!(
                            request = %hex(&payload),
                            response = %hex(&response),
                            "sdp exchange",
                        );
                        outbound.push((cid, response));
                    } else if psm == Psm::AVDTP {
                        if Some(cid) == link.avdtp_media {
                            self.on_media(link, payload).await;
                        } else {
                            self.on_avdtp(link, cid, &payload, sink, &mut outbound)
                                .await?;
                        }
                    } else if psm == Psm::AVCTP {
                        self.on_avctp(link, cid, &payload, sink, &mut outbound)
                            .await?;
                    }
                }

                L2capEvent::ConnectFailed { psm, result } => {
                    warn!(%psm, ?result, "outgoing l2cap connect refused");
                }
                // L2capEvent is #[non_exhaustive]; a new variant must be noticed rather
                // than dropped, since every existing one is load-bearing.
                other => debug!(?other, "unhandled l2cap event"),
            }
        }

        for pdu in signalling {
            acl.send(handle, pdu);
        }
        // `Multiplexer::send` is the only thing that knows which identifier the peer uses
        // for a channel. Addressing a reply with our own is invisible whenever both ends
        // happen to allocate the same number — which BlueZ did, and an iPhone does not.
        for (cid, payload) in outbound {
            match link.mux.send(cid, payload) {
                Ok(events) => {
                    for event in events {
                        if let L2capEvent::Send(pdu) = event {
                            acl.send(handle, pdu);
                        }
                    }
                }
                Err(e) => warn!(error = %e, %cid, "dropping a reply we cannot address"),
            }
        }
        Ok(())
    }

    /// AVDTP signaling: drive the sink session and act on what it reports.
    async fn on_avdtp(
        &self,
        link: &mut Link,
        cid: Cid,
        payload: &[u8],
        sink: &SessionSink,
        outbound: &mut Vec<(Cid, Bytes)>,
    ) -> Result<(), CoreError> {
        let msg = match Message::decode(payload) {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %e, "undecodable AVDTP message");
                return Ok(());
            }
        };
        for event in link.sink.handle(&msg) {
            match event {
                SinkEvent::Reply(reply) => outbound.push((cid, reply.encode())),
                SinkEvent::Configured {
                    codec,
                    format,
                    configuration,
                } => {
                    info!(?codec, %format, "bluetooth: stream configured");
                    link.audio_format = Some(format);
                    link.depacketizer = Some(Depacketizer::new(codec, format.sample_rate()));
                    link.description = std::mem::take(&mut link.description)
                        .merged(SourceDescription::new().with_link(configuration.describe()));
                }
                SinkEvent::Started => {
                    // START cannot precede SET_CONFIGURATION in the sink state machine,
                    // so a missing format means a bug here rather than a sender problem —
                    // and starting a session without one would decode at a guessed rate,
                    // which is exactly what Q25 was.
                    let Some(format) = link.audio_format else {
                        warn!("bluetooth: stream started with no negotiated format");
                        continue;
                    };
                    if !link.session_open {
                        let (tx, rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
                        link.audio_tx = Some(tx);
                        link.session_open = true;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Audio {
                                source: FrameSource::Encoded(rx),
                                format,
                            })
                            .await?;
                        // Only now can the description be delivered: the session
                        // manager rejects source info for a source that is not active,
                        // and this is the moment it becomes active.
                        link_sink
                            .emit(SessionEvent::SourceInfo(link.description.clone()))
                            .await?;
                    }
                }
                SinkEvent::Closed => {
                    link.audio_tx = None;
                    link.depacketizer = None;
                    link.audio_format = None;
                    if link.session_open {
                        link.session_open = false;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink.emit(SessionEvent::End).await?;
                    }
                }
                SinkEvent::Opened | SinkEvent::Suspended => {}
            }
        }
        Ok(())
    }

    /// A media packet: depacketize and push the frame at the pipeline.
    async fn on_media(&self, link: &mut Link, payload: Bytes) {
        let (Some(depacketizer), Some(tx)) = (link.depacketizer.as_mut(), link.audio_tx.as_ref())
        else {
            return;
        };
        match depacketizer.push(payload) {
            Ok(frame) => {
                // `try_send` rather than `send`: blocking here would stall the whole
                // adapter, including the signaling channel, so a phone could not even
                // pause. A full queue means decode is behind, which is worth saying.
                if tx.try_send(frame).is_err() {
                    warn!("audio queue full; dropping a frame");
                }
            }
            Err(e) => debug!(error = %e, "bad media packet"),
        }
    }

    /// AVCTP: metadata responses and volume commands.
    async fn on_avctp(
        &self,
        link: &mut Link,
        cid: Cid,
        payload: &[u8],
        sink: &SessionSink,
        outbound: &mut Vec<(Cid, Bytes)>,
    ) -> Result<(), CoreError> {
        let Ok(msg) = AvctpMessage::decode(payload) else {
            return Ok(());
        };
        let Ok(frame) = AvcFrame::decode(&msg.body) else {
            return Ok(());
        };
        let Ok(vendor) = avrcp::VendorPdu::parse(&frame.operands) else {
            return Ok(());
        };

        match vendor.pdu_id {
            avrcp::pdu::GET_ELEMENT_ATTRIBUTES if !frame.ctype.is_failure() => {
                if let Ok(parsed) = avrcp::parse_element_attributes(&vendor.parameters) {
                    let changed = !parsed.now_playing.is_same_item(&link.now_playing);
                    link.now_playing = parsed.now_playing.clone();
                    if link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::NowPlaying(parsed.now_playing))
                            .await?;
                    }
                    if changed {
                        if let Some(handle) = parsed.cover_art_handle {
                            // The fetch runs on its own L2CAP channel to the PSM in the
                            // peer's SDP record; the text card is already on screen and
                            // the art lands as a second snapshot.
                            debug!(handle, "bluetooth: cover art available");
                        }
                    }
                }
            }
            avrcp::pdu::SET_ABSOLUTE_VOLUME => {
                // Q24: the phone is authoritative. Accept and mirror it.
                if let Some(&raw) = vendor.parameters.first() {
                    let fraction = avrcp::volume_to_fraction(raw);
                    if link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Control(castaway_core::ControlTxn::Volume(
                                fraction,
                            )))
                            .await?;
                    }
                    // Echo the accepted value back, or the phone's volume UI sticks.
                    let response = avrcp::vendor_command(
                        Ctype::Accepted,
                        avrcp::pdu::SET_ABSOLUTE_VOLUME,
                        &[raw & 0x7F],
                    );
                    outbound.push((
                        cid,
                        AvctpMessage::response(&msg, response.encode()).encode(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Pump [`AvrcpControl`] frames onto the AVCTP channel.
    ///
    /// Queues through the same [`AclWriter`] as everything else rather than writing to
    /// the transport directly: two tasks fragmenting onto one handle would interleave
    /// their fragments, and basic-mode L2CAP has no way to sort that out (Q26).
    fn spawn_control_writer(
        handle: ConnectionHandle,
        cid: Cid,
        mut rx: mpsc::Receiver<AvcFrame>,
        acl: AclWriter,
    ) {
        tokio::spawn(async move {
            let mut transaction = 0u8;
            while let Some(frame) = rx.recv().await {
                acl.send(handle, avctp_pdu(cid, transaction, &frame));
                transaction = (transaction + 1) & 0x0F;
            }
        });
    }
}

/// Hex for a log line, truncated so a big record does not swamp the journal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes.iter().take(256) {
        let _ = write!(out, "{b:02x}");
    }
    if bytes.len() > 256 {
        let _ = write!(out, "…({} bytes)", bytes.len());
    }
    out
}

/// Wrap an AV/C frame in AVCTP, ready for an L2CAP channel.
fn avctp_body(transaction: u8, frame: &AvcFrame) -> Bytes {
    AvctpMessage::command(transaction, frame.encode()).encode()
}

/// Wrap an AV/C frame in AVCTP and an L2CAP PDU addressed to `peer_cid`.
fn avctp_pdu(peer_cid: Cid, transaction: u8, frame: &AvcFrame) -> L2capPdu {
    L2capPdu::new(peer_cid, avctp_body(transaction, frame))
}

/// Re-exported for the adapter's tests and the app's wiring.
pub use avdtp::Signal as AvdtpSignal;
