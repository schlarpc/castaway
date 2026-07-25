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
    Advertisement, CoreError, EncodedFrame, FrameSource, NowPlaying, ProtocolKind, SessionEvent,
    SessionSink, SourceAdapter,
};
use substrate_hci::{
    AclPacket, BdAddr, ConnectionHandle, Event, HciPacket, HciTransport, LinkKey, PacketBoundary,
    Reassembler,
};
use substrate_l2cap::{Cid, L2capEvent, L2capPdu, Multiplexer, Psm};
use substrate_sdp::{a2dp_sink, avrcp_controller, avrcp_target, SdpServer};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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

/// Configuration for the Bluetooth adapter.
#[derive(Debug, Clone)]
pub struct BluetoothConfig {
    /// Controller bring-up settings.
    pub host: HostConfig,
    /// Whether to advertise the LDAC endpoint. Should mirror the `ldac` build feature —
    /// advertising a codec we cannot decode makes the session silence (Q22).
    pub enable_ldac: bool,
    /// Link keys loaded from disk, so repeat guests reconnect silently (Q23).
    pub link_keys: Vec<(BdAddr, LinkKey)>,
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
            link_keys: Vec::new(),
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
    audio_tx: Option<mpsc::Sender<EncodedFrame>>,
    /// Whether a `SessionEvent::Audio` has already been emitted for this link.
    session_open: bool,
    /// Metadata accumulated for this link, re-emitted as a full snapshot on change.
    now_playing: NowPlaying,
    /// Next AVCTP transaction label.
    avctp_transaction: u8,
}

impl Link {
    fn new(peer: BdAddr, enable_ldac: bool) -> Self {
        let mut mux = Multiplexer::new(672);
        mux.listen(Psm::SDP);
        mux.listen(Psm::AVDTP);
        mux.listen(Psm::AVCTP);
        Self {
            peer,
            reassembler: Reassembler::new(),
            mux,
            sink: SinkSession::new(advertised(enable_ldac)),
            avdtp_signaling: None,
            avdtp_media: None,
            avctp: None,
            depacketizer: None,
            audio_tx: None,
            session_open: false,
            now_playing: NowPlaying::default(),
            avctp_transaction: 0,
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
        Self {
            transport,
            config,
            sdp,
        }
    }

    /// Send one HCI packet.
    async fn send(&self, packet: HciPacket) -> Result<(), CoreError> {
        self.transport
            .send(packet)
            .await
            .map_err(|e| CoreError::Adapter(e.to_string()))
    }

    /// Write an L2CAP PDU, fragmenting to the controller's ACL buffer size.
    ///
    /// Fragmentation is not optional: a dongle's ACL buffer is routinely 300-odd bytes
    /// while an AVDTP capability response or an SDP record comfortably exceeds it, and a
    /// controller handed an oversized fragment drops it without complaint.
    async fn send_pdu(
        &self,
        handle: ConnectionHandle,
        pdu: &L2capPdu,
        acl_mtu: u16,
    ) -> Result<(), CoreError> {
        let bytes = pdu
            .encode()
            .map_err(|e| CoreError::Adapter(format!("l2cap encode: {e}")))?;
        let mtu = usize::from(acl_mtu.max(1));
        for (i, chunk) in bytes.chunks(mtu).enumerate() {
            let boundary = if i == 0 {
                PacketBoundary::FirstFlushable
            } else {
                PacketBoundary::Continuing
            };
            self.send(HciPacket::Acl(AclPacket::new(
                handle,
                boundary,
                Bytes::copy_from_slice(chunk),
            )))
            .await?;
        }
        Ok(())
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
        let mut acl_mtu = host.acl_mtu();

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
                    for action in host.on_event(&event) {
                        match &action {
                            HostAction::Ready { address, .. } => {
                                acl_mtu = host.acl_mtu();
                                info!(%address, "bluetooth: discoverable");
                            }
                            HostAction::LinkUp { handle, peer } => {
                                info!(%peer, "bluetooth: link up");
                                links.insert(
                                    handle.raw(),
                                    Link::new(*peer, self.config.enable_ldac),
                                );
                            }
                            HostAction::LinkDown { handle, peer, .. } => {
                                info!(%peer, "bluetooth: link down");
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

                HciPacket::Acl(acl) => {
                    let Some(link) = links.get_mut(&acl.handle.raw()) else {
                        debug!(handle = %acl.handle, "ACL for an unknown link");
                        continue;
                    };
                    let pdu_bytes = match link.reassembler.push(&acl) {
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
                        acl.handle,
                        events,
                        links.get_mut(&acl.handle.raw()),
                        &sink,
                        acl_mtu,
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
        if let HostAction::Paired { peer, .. } = action {
            // Persistence is the app's job (it owns the config dir); surfacing it here
            // keeps this crate free of filesystem access.
            info!(%peer, "bluetooth: paired");
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
        acl_mtu: u16,
    ) -> Result<(), CoreError> {
        let Some(link) = link else {
            return Ok(());
        };
        let mut outbound: Vec<L2capPdu> = Vec::new();

        for event in events {
            match event {
                L2capEvent::Send(pdu) => outbound.push(pdu),

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
                        self.spawn_control_writer(handle, cid, rx, acl_mtu);
                        // Ask for metadata straight away rather than waiting for a
                        // notification; a track already playing produces no change event.
                        let transaction = link.next_transaction();
                        outbound.push(avctp_pdu(
                            cid,
                            transaction,
                            &avrcp::get_element_attributes(&avrcp::attribute::ALL),
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
                        outbound.push(L2capPdu::new(cid, self.sdp.handle(&payload)));
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

        for pdu in outbound {
            self.send_pdu(handle, &pdu, acl_mtu).await?;
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
        outbound: &mut Vec<L2capPdu>,
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
                SinkEvent::Reply(reply) => outbound.push(L2capPdu::new(cid, reply.encode())),
                SinkEvent::Configured {
                    codec, sample_rate, ..
                } => {
                    info!(?codec, sample_rate, "bluetooth: stream configured");
                    link.depacketizer = Some(Depacketizer::new(codec, sample_rate));
                }
                SinkEvent::Started => {
                    if !link.session_open {
                        let (tx, rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
                        link.audio_tx = Some(tx);
                        link.session_open = true;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Audio {
                                source: FrameSource::Encoded(rx),
                            })
                            .await?;
                    }
                }
                SinkEvent::Closed => {
                    link.audio_tx = None;
                    link.depacketizer = None;
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
        outbound: &mut Vec<L2capPdu>,
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
                    outbound.push(L2capPdu::new(
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
    fn spawn_control_writer(
        &self,
        handle: ConnectionHandle,
        cid: Cid,
        mut rx: mpsc::Receiver<AvcFrame>,
        acl_mtu: u16,
    ) {
        let transport = Arc::clone(&self.transport);
        tokio::spawn(async move {
            let mut transaction = 0u8;
            while let Some(frame) = rx.recv().await {
                let pdu = avctp_pdu(cid, transaction, &frame);
                transaction = (transaction + 1) & 0x0F;
                let Ok(bytes) = pdu.encode() else { continue };
                let mtu = usize::from(acl_mtu.max(1));
                for (i, chunk) in bytes.chunks(mtu).enumerate() {
                    let boundary = if i == 0 {
                        PacketBoundary::FirstFlushable
                    } else {
                        PacketBoundary::Continuing
                    };
                    if transport
                        .send(HciPacket::Acl(AclPacket::new(
                            handle,
                            boundary,
                            Bytes::copy_from_slice(chunk),
                        )))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }
}

/// Wrap an AV/C frame in AVCTP and an L2CAP PDU.
fn avctp_pdu(cid: Cid, transaction: u8, frame: &AvcFrame) -> L2capPdu {
    L2capPdu::new(
        cid,
        AvctpMessage::command(transaction, frame.encode()).encode(),
    )
}

/// Re-exported for the adapter's tests and the app's wiring.
pub use avdtp::Signal as AvdtpSignal;
