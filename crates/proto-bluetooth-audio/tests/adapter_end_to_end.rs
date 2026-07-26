//! A scripted phone drives the whole adapter, from controller reset to audio frames.
//!
//! This is the test that proves the layers compose: HCI bring-up, pairing, L2CAP channel
//! setup, SDP, AVDTP negotiation and media all run through the real code, against a
//! [`ScriptedTransport`] instead of a radio. If this passes, the only thing standing
//! between it and sound is a dongle and a decoder.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use castaway_core::{
    AudioCodec, FrameSource, ProtocolKind, SessionEvent, SessionSink, SourceAdapter, SourceId,
    SourceMessage,
};
use proto_bluetooth_audio::adapter::{BluetoothAdapter, BluetoothConfig};
use proto_bluetooth_audio::avdtp::{Message, Seid, Signal};
use proto_bluetooth_audio::codec::{ChannelModes, CodecCapability, SampleRates};
use substrate_hci::{
    event::code, AclPacket, BdAddr, ConnectionHandle, HciPacket, HciTransport, PacketBoundary,
    ScriptedTransport, Status,
};
use substrate_l2cap::{Cid, L2capPdu, Psm, Signal as L2capSignal};
use tokio::sync::mpsc;

const PEER: &str = "AA:BB:CC:DD:EE:FF";
const HANDLE: u16 = 0x000B;

/// A transport that answers every command with a plausible completion, so bring-up
/// proceeds without the test scripting eight round trips by hand.
fn transport() -> Arc<ScriptedTransport> {
    controller(8, true)
}

/// Build a scripted controller with `acl_packets` ACL buffers.
///
/// `report_completions` decides whether it hands those buffers back the way a real
/// controller does. Withholding them is how the flow-control path is exercised: a
/// controller with infinite buffers is a fiction, and it is the fiction under which an
/// unpaced writer looks like it works (Q26).
fn controller(acl_packets: u16, report_completions: bool) -> Arc<ScriptedTransport> {
    Arc::new(ScriptedTransport::new().with_responder(move |sent| {
        // A real controller frees each ACL buffer and says so. Nothing else ever returns
        // a credit, so a host that ignores this event stalls after `acl_packets` writes.
        if let HciPacket::Acl(acl) = sent {
            if !report_completions {
                return Vec::new();
            }
            let mut params = vec![0x01];
            params.extend_from_slice(&acl.handle.raw().to_le_bytes());
            params.extend_from_slice(&1u16.to_le_bytes());
            return vec![HciPacket::Event {
                code: code::NUMBER_OF_COMPLETED_PACKETS,
                params: Bytes::from(params),
            }];
        }
        let HciPacket::Command { opcode, .. } = sent else {
            return Vec::new();
        };
        let mut params = vec![0x01];
        params.extend_from_slice(&opcode.raw().to_le_bytes());
        match *opcode {
            substrate_hci::OpCode::READ_BUFFER_SIZE => {
                // 340-byte ACL buffer — a typical dongle, and small enough that
                // fragmentation is exercised rather than skipped.
                params.extend_from_slice(&[0x00, 0x54, 0x01, 0xff]);
                params.extend_from_slice(&acl_packets.to_le_bytes());
                params.extend_from_slice(&[0x08, 0x00]);
            }
            substrate_hci::OpCode::READ_BD_ADDR => {
                params.extend_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
            }
            substrate_hci::OpCode::ACCEPT_CONNECTION_REQUEST => {
                // Command status, then the link coming up.
                let addr: BdAddr = PEER.parse().unwrap();
                let mut complete = vec![Status::SUCCESS.0];
                complete.extend_from_slice(&HANDLE.to_le_bytes());
                complete.extend_from_slice(&addr.to_wire());
                complete.push(0x01); // ACL
                complete.push(0x00); // encryption off
                return vec![
                    HciPacket::Event {
                        code: code::COMMAND_STATUS,
                        params: Bytes::from(params),
                    },
                    HciPacket::Event {
                        code: code::CONNECTION_COMPLETE,
                        params: Bytes::from(complete),
                    },
                ];
            }
            _ => params.push(0x00),
        }
        vec![HciPacket::Event {
            code: code::COMMAND_COMPLETE,
            params: Bytes::from(params),
        }]
    }))
}

/// Poll until `f` returns something, or fail the test.
async fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..2000 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Every complete L2CAP PDU the adapter has written, reassembled from ACL fragments.
fn sent_pdus(transport: &ScriptedTransport) -> Vec<L2capPdu> {
    let mut reassembler = substrate_hci::Reassembler::new();
    let mut out = Vec::new();
    for packet in transport.sent() {
        if let HciPacket::Acl(acl) = packet {
            if let Ok(Some(bytes)) = reassembler.push(&acl) {
                if let Ok(pdu) = L2capPdu::decode(&bytes) {
                    out.push(pdu);
                }
            }
        }
    }
    out
}

/// Feed one complete L2CAP PDU to the adapter as a single ACL fragment.
fn push_pdu(transport: &ScriptedTransport, pdu: &L2capPdu) {
    transport.push(HciPacket::Acl(AclPacket::new(
        ConnectionHandle::new(HANDLE).unwrap(),
        PacketBoundary::FirstFlushable,
        pdu.encode().unwrap(),
    )));
}

/// Drive the adapter to the point where an ACL link exists, returning the event stream.
async fn connected() -> (Arc<ScriptedTransport>, mpsc::Receiver<SourceMessage>) {
    connected_to(transport()).await
}

/// The same, against a controller the caller built.
async fn connected_to(
    transport: Arc<ScriptedTransport>,
) -> (Arc<ScriptedTransport>, mpsc::Receiver<SourceMessage>) {
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(&transport) as Arc<dyn HciTransport>,
        BluetoothConfig {
            enable_ldac: true,
            ..BluetoothConfig::default()
        },
    ));
    let (tx, rx) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Bluetooth, "listener"), tx);
    tokio::spawn(Arc::clone(&adapter).run(sink));

    // Bring-up runs itself off the auto-responder; wait for scan enable, which is last.
    eventually("scan enable", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::WRITE_SCAN_ENABLE)
            .then_some(())
    })
    .await;

    // The phone pages us.
    let addr: BdAddr = PEER.parse().unwrap();
    let mut params = addr.to_wire().to_vec();
    params.extend_from_slice(&[0x0C, 0x02, 0x5A]); // class of device
    params.push(0x01); // ACL
    transport.push(HciPacket::Event {
        code: code::CONNECTION_REQUEST,
        params: Bytes::from(params),
    });
    eventually("connection accepted", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::ACCEPT_CONNECTION_REQUEST)
            .then_some(())
    })
    .await;
    (transport, rx)
}

/// Open an L2CAP channel to `psm` from the phone side. Returns (our cid, their cid).
async fn open_channel(transport: &ScriptedTransport, psm: Psm, phone_cid: u16) -> (Cid, Cid) {
    let before = sent_pdus(transport).len();
    push_pdu(
        transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConnectionRequest {
                id: 1,
                psm,
                source_cid: Cid::new(phone_cid),
            }
            .encode()
            .unwrap(),
        ),
    );

    // The adapter answers with a connection response and its own configuration request.
    let sink_cid = eventually("connection response", || {
        sent_pdus(transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
            .flatten()
            .find_map(|sig| match sig {
                L2capSignal::ConnectionResponse { dest_cid, .. } => Some(dest_cid),
                _ => None,
            })
    })
    .await;

    // Accept its configuration, and configure our own direction.
    push_pdu(
        transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConfigurationResponse {
                id: 2,
                source_cid: sink_cid,
                flags: 0,
                result: substrate_l2cap::ConfigResult::Success,
                options: vec![],
            }
            .encode()
            .unwrap(),
        ),
    );
    push_pdu(
        transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConfigurationRequest {
                id: 3,
                dest_cid: sink_cid,
                flags: 0,
                options: vec![substrate_l2cap::ConfigOption::Mtu(672)],
            }
            .encode()
            .unwrap(),
        ),
    );
    (sink_cid, Cid::new(phone_cid))
}

/// Send an AVDTP command on `cid` and wait for the reply.
async fn avdtp(
    transport: &ScriptedTransport,
    cid: Cid,
    transaction: u8,
    signal: Signal,
    payload: &[u8],
) -> Message {
    let before = sent_pdus(transport).len();
    push_pdu(
        transport,
        &L2capPdu::new(
            cid,
            Message::command(transaction, signal, Bytes::copy_from_slice(payload)).encode(),
        ),
    );
    eventually("avdtp reply", || {
        sent_pdus(transport)
            .into_iter()
            .skip(before)
            .filter(|pdu| pdu.cid == Cid::new(0x0040) || pdu.cid.is_dynamic())
            .filter_map(|pdu| Message::decode(&pdu.payload).ok())
            .find(|m| m.signal == signal && m.transaction == transaction)
    })
    .await
}

#[tokio::test]
async fn the_adapter_brings_a_controller_up_and_becomes_discoverable() {
    let transport = transport();
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(&transport) as Arc<dyn HciTransport>,
        BluetoothConfig::default(),
    ));
    let (tx, _rx) = mpsc::channel(16);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Bluetooth, "listener"), tx);
    tokio::spawn(Arc::clone(&adapter).run(sink));

    eventually("bring-up", || {
        let cmds = transport.sent_commands();
        cmds.contains(&substrate_hci::OpCode::WRITE_SCAN_ENABLE)
            .then_some(())
    })
    .await;

    let cmds = transport.sent_commands();
    assert_eq!(cmds.first(), Some(&substrate_hci::OpCode::RESET));
    assert!(cmds.contains(&substrate_hci::OpCode::WRITE_SIMPLE_PAIRING_MODE));
    assert!(cmds.contains(&substrate_hci::OpCode::WRITE_CLASS_OF_DEVICE));
}

#[tokio::test]
async fn a_phone_pairs_without_any_prompt() {
    // Q23 end to end: the controller asks, the adapter answers, nobody is prompted.
    let (transport, _rx) = connected().await;
    let addr: BdAddr = PEER.parse().unwrap();

    transport.push(HciPacket::Event {
        code: code::IO_CAPABILITY_REQUEST,
        params: Bytes::from(addr.to_wire().to_vec()),
    });
    eventually("io capability reply", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::IO_CAPABILITY_REQUEST_REPLY)
            .then_some(())
    })
    .await;

    let mut confirm = addr.to_wire().to_vec();
    confirm.extend_from_slice(&123_456u32.to_le_bytes());
    transport.push(HciPacket::Event {
        code: code::USER_CONFIRMATION_REQUEST,
        params: Bytes::from(confirm),
    });
    eventually("user confirmation accepted", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::USER_CONFIRMATION_REQUEST_REPLY)
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn sdp_answers_a_query_for_our_sink_record() {
    // The first thing a phone does after connecting. If this record is wrong it walks
    // away without ever trying AVDTP.
    let (transport, _rx) = connected().await;
    let (cid, _) = open_channel(&transport, Psm::SDP, 0x0040).await;

    let before = sent_pdus(&transport).len();
    let request = substrate_sdp::SdpRequest::ServiceSearchAttribute {
        tid: 1,
        patterns: vec![substrate_sdp::Uuid::AUDIO_SINK],
        max_bytes: 672,
        attributes: vec![substrate_sdp::AttributeRange::Range(0x0000, 0xFFFF)],
        cont: substrate_sdp::Continuation::none(),
    };
    push_pdu(&transport, &L2capPdu::new(cid, request.encode()));

    let response = eventually("sdp response", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .find_map(|pdu| substrate_sdp::SdpResponse::decode(&pdu.payload).ok())
    })
    .await;

    let substrate_sdp::SdpResponse::ServiceSearchAttribute { lists, .. } = response else {
        panic!("expected a search-attribute response");
    };
    let records = substrate_sdp::parse_records(&lists).unwrap();
    assert!(!records.is_empty(), "the sink record must be findable");
    assert_eq!(
        records[0].l2cap_psm(substrate_sdp::record::attr::PROTOCOL_DESCRIPTOR_LIST),
        Some(0x0019),
        "the record must point at the AVDTP PSM"
    );
}

/// How many ACL fragments the host has written.
fn acl_count(transport: &ScriptedTransport) -> usize {
    transport
        .sent()
        .iter()
        .filter(|p| matches!(p, HciPacket::Acl(_)))
        .count()
}

/// Whether the host has written an L2CAP configuration *response* — the one PDU whose
/// loss leaves the peer's half of a channel unconfigured forever.
fn sent_a_config_response(transport: &ScriptedTransport) -> bool {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
        .flatten()
        .any(|sig| matches!(sig, L2capSignal::ConfigurationResponse { .. }))
}

#[tokio::test]
async fn writes_wait_for_controller_buffers_instead_of_vanishing_into_them() {
    // Q26, reproduced: a controller with two ACL buffers that has not yet freed either.
    // An unpaced host writes a third fragment anyway, the controller discards it without
    // a word, and the peer waits for a reply that this end believes it sent. On the
    // bench that lost fragment was an L2CAP configuration response, so BlueZ never
    // finished configuring the media channel, never sent AVDTP START, and the link
    // idled out with nothing in any log.
    let transport = controller(2, false);
    let (transport, _rx) = connected_to(transport).await;

    // Opening a channel costs exactly the pool: a connection response and our own
    // configuration request, one fragment each.
    push_pdu(
        &transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConnectionRequest {
                id: 1,
                psm: Psm::AVDTP,
                source_cid: Cid::new(0x0040),
            }
            .encode()
            .unwrap(),
        ),
    );
    eventually("the pool to be spent", || {
        (acl_count(&transport) == 2).then_some(())
    })
    .await;

    // Now the phone configures its direction. The reply is the third fragment, and there
    // is no buffer for it.
    let sink_cid = sent_pdus(&transport)
        .into_iter()
        .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
        .flatten()
        .find_map(|sig| match sig {
            L2capSignal::ConnectionResponse { dest_cid, .. } => Some(dest_cid),
            _ => None,
        })
        .expect("the channel must have been accepted");
    push_pdu(
        &transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConfigurationRequest {
                id: 3,
                dest_cid: sink_cid,
                flags: 0,
                options: vec![substrate_l2cap::ConfigOption::Mtu(672)],
            }
            .encode()
            .unwrap(),
        ),
    );

    // Give it every chance to write the fragment it must not write.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        acl_count(&transport),
        2,
        "the host must never have more fragments outstanding than the controller has \
         buffers; the extra one is discarded, not queued"
    );
    assert!(
        !sent_a_config_response(&transport),
        "the response must be held, not written into a buffer that does not exist"
    );

    // The controller frees both buffers. The held reply must now go out — waiting is
    // only correct if it is waiting, not dropping.
    let mut params = vec![0x01];
    params.extend_from_slice(&HANDLE.to_le_bytes());
    params.extend_from_slice(&2u16.to_le_bytes());
    transport.push(HciPacket::Event {
        code: code::NUMBER_OF_COMPLETED_PACKETS,
        params: Bytes::from(params),
    });

    eventually("the held configuration response", || {
        sent_a_config_response(&transport).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_dropped_link_gives_its_controller_buffers_back() {
    // Those fragments are flushed by the controller and never reported complete, so
    // without reclaiming them the pool shrinks by one credit per phone that leaves
    // mid-write — and a kiosk that has run for a week eventually stops answering at all.
    let transport = controller(2, false);
    let (transport, _rx) = connected_to(transport).await;

    push_pdu(
        &transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConnectionRequest {
                id: 1,
                psm: Psm::AVDTP,
                source_cid: Cid::new(0x0040),
            }
            .encode()
            .unwrap(),
        ),
    );
    eventually("the pool to be spent", || {
        (acl_count(&transport) == 2).then_some(())
    })
    .await;

    let mut params = vec![Status::SUCCESS.0];
    params.extend_from_slice(&HANDLE.to_le_bytes());
    params.push(Status::REMOTE_USER_TERMINATED.0);
    transport.push(HciPacket::Event {
        code: code::DISCONNECTION_COMPLETE,
        params: Bytes::from(params),
    });

    // A second phone connects. With the buffers reclaimed it must be answered; without,
    // the pool is permanently two credits smaller and this write never happens.
    let addr: BdAddr = PEER.parse().unwrap();
    let mut params = addr.to_wire().to_vec();
    params.extend_from_slice(&[0x0C, 0x02, 0x5A]);
    params.push(0x01);
    transport.push(HciPacket::Event {
        code: code::CONNECTION_REQUEST,
        params: Bytes::from(params),
    });
    eventually("the second link", || {
        (transport
            .sent_commands()
            .iter()
            .filter(|c| **c == substrate_hci::OpCode::ACCEPT_CONNECTION_REQUEST)
            .count()
            == 2)
            .then_some(())
    })
    .await;
    push_pdu(
        &transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConnectionRequest {
                id: 1,
                psm: Psm::AVDTP,
                source_cid: Cid::new(0x0040),
            }
            .encode()
            .unwrap(),
        ),
    );
    eventually("the new link to be answered", || {
        (acl_count(&transport) > 2).then_some(())
    })
    .await;
}

#[tokio::test]
async fn replies_are_addressed_with_the_peers_channel_id_not_our_own() {
    // Each end of an L2CAP channel allocates its own identifier, and an outgoing packet
    // carries the *receiver's*. Reusing the id the inbound event named is invisible for
    // as long as both ends happen to pick the same number — which BlueZ does, because it
    // allocates from 0x0040 exactly like we do. An iPhone does not, and every reply we
    // sent addressed a channel it had never heard of: it waited for an answer that never
    // came, timed out after seven seconds, and hung up before ever trying to pair.
    //
    // The phone's id here is deliberately far from ours so the two cannot coincide.
    const PHONE_CID: u16 = 0x00F1;
    let (transport, _rx) = connected().await;
    let (ours, theirs) = open_channel(&transport, Psm::SDP, PHONE_CID).await;
    assert_ne!(
        ours.raw(),
        theirs.raw(),
        "the test is worthless unless the two ends disagree about the id"
    );

    let before = sent_pdus(&transport).len();
    let request = substrate_sdp::SdpRequest::ServiceSearchAttribute {
        tid: 7,
        patterns: vec![substrate_sdp::Uuid::AUDIO_SINK],
        max_bytes: 672,
        attributes: vec![substrate_sdp::AttributeRange::Range(0x0000, 0xFFFF)],
        cont: substrate_sdp::Continuation::none(),
    };
    push_pdu(&transport, &L2capPdu::new(ours, request.encode()));

    let reply = eventually("the sdp reply", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .find(|pdu| substrate_sdp::SdpResponse::decode(&pdu.payload).is_ok())
    })
    .await;
    assert_eq!(
        reply.cid.raw(),
        PHONE_CID,
        "a reply must carry the peer's channel id; ours means the peer never sees it"
    );
}

#[tokio::test]
async fn avdtp_replies_also_use_the_peers_channel_id() {
    // Same bug, the path that actually carries audio: an AVDTP reply sent to our own id
    // means the sender never learns what we support and the stream never starts.
    const PHONE_CID: u16 = 0x00E7;
    let (transport, _rx) = connected().await;
    let (ours, _) = open_channel(&transport, Psm::AVDTP, PHONE_CID).await;

    let before = sent_pdus(&transport).len();
    push_pdu(
        &transport,
        &L2capPdu::new(
            ours,
            Message::command(1, Signal::Discover, Bytes::new()).encode(),
        ),
    );
    let reply = eventually("the avdtp reply", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .find(|pdu| Message::decode(&pdu.payload).is_ok_and(|m| m.signal == Signal::Discover))
    })
    .await;
    assert_eq!(
        reply.cid.raw(),
        PHONE_CID,
        "an AVDTP reply must reach the peer's channel, not ours"
    );
}

#[tokio::test]
async fn a_full_stream_reaches_the_pipeline_as_audio_frames() {
    // The whole point. A phone connects, negotiates aptX, opens a media channel, and
    // pushes packets — and the session manager sees an audio session with real frames.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    // Discover, then pick the aptX endpoint.
    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = eventually("an aptX endpoint", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .nth(2) // ldac, aptx-hd, aptx — the table's preference order
    })
    .await;

    let caps = avdtp(
        &transport,
        signaling,
        2,
        Signal::GetAllCapabilities,
        &[seid.shifted()],
    )
    .await;
    let advertised = proto_bluetooth_audio::avdtp::find_codec_capability(&caps.payload).unwrap();
    assert_eq!(advertised.audio_codec(), AudioCodec::AptX);

    // Configure it down to one rate and one channel mode. 48 kHz on purpose: it is what
    // BlueZ actually picked on hardware, and it is not the rate a defaulted decoder would
    // have guessed — which is the whole of Q25.
    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let reply = avdtp(&transport, signaling, 3, Signal::SetConfiguration, &set).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "configuration should be accepted"
    );

    avdtp(&transport, signaling, 4, Signal::Open, &[seid.shifted()]).await;

    // The media transport is a *second* L2CAP channel on the same PSM.
    let (media, _) = open_channel(&transport, Psm::AVDTP, 0x0041).await;
    assert_ne!(media, signaling, "media must be its own channel");

    avdtp(&transport, signaling, 5, Signal::Start, &[seid.shifted()]).await;

    // The session manager should now see an audio session.
    let msg = eventually("an audio session event", || rx.try_recv().ok()).await;
    let SessionEvent::Audio { source, format } = msg.event else {
        panic!("expected an audio session, got {:?}", msg.event);
    };
    // The negotiated rate must reach the pipeline, not a default. aptX has no in-band
    // rate, so getting this wrong plays the stream ~9% slow and logs nothing (Q25).
    assert_eq!(
        format.sample_rate(),
        48_000,
        "the negotiated rate must survive"
    );
    assert_eq!(format.channels(), 2);
    let FrameSource::Encoded(mut frames) = source else {
        panic!("audio must arrive as encoded frames");
    };
    assert_eq!(msg.source.kind, ProtocolKind::Bluetooth);
    assert_eq!(&*msg.source.instance, PEER);

    // Classic aptX carries no RTP header, so the packet *is* the codec payload.
    let mut payload = BytesMut::new();
    payload.put_slice(&[0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]);
    push_pdu(&transport, &L2capPdu::new(media, payload.freeze()));

    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("a media packet should become a frame")
        .expect("frame channel should be open");
    assert_eq!(frame.audio_codec, Some(AudioCodec::AptX));
    assert_eq!(
        &frame.data[..],
        &[0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28],
        "every byte of an aptX packet is audio"
    );
}

#[tokio::test]
async fn a_dropped_link_ends_the_session() {
    // The phone walks out mid-song. No teardown handshake, just a dead link — and the
    // session manager must be told, or the panel keeps showing a card forever.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = Seid::from_shifted(discover.payload[4]).unwrap(); // the aptX endpoint
    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(&transport, signaling, 2, Signal::SetConfiguration, &set).await;
    avdtp(&transport, signaling, 3, Signal::Open, &[seid.shifted()]).await;
    avdtp(&transport, signaling, 4, Signal::Start, &[seid.shifted()]).await;
    eventually("audio session", || rx.try_recv().ok()).await;

    let mut params = vec![Status::SUCCESS.0];
    params.extend_from_slice(&HANDLE.to_le_bytes());
    params.push(Status::REMOTE_USER_TERMINATED.0);
    transport.push(HciPacket::Event {
        code: code::DISCONNECTION_COMPLETE,
        params: Bytes::from(params),
    });

    // Drain past whatever else the session emitted — source info arrives around the
    // same time — and assert End actually lands.
    eventually("session end", || match rx.try_recv() {
        Ok(msg) if matches!(msg.event, SessionEvent::End) => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn the_device_and_its_codec_are_reported_for_the_screen() {
    // What the panel shows above the now-playing card: which phone, and what was
    // negotiated. The address is known at link-up and the codec at configuration, so
    // this also checks the two are merged rather than one overwriting the other.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = Seid::from_shifted(discover.payload[4]).unwrap(); // the aptX endpoint
    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(&transport, signaling, 2, Signal::SetConfiguration, &set).await;
    avdtp(&transport, signaling, 3, Signal::Open, &[seid.shifted()]).await;
    avdtp(&transport, signaling, 4, Signal::Start, &[seid.shifted()]).await;

    let info = eventually("source info", || match rx.try_recv() {
        Ok(msg) => match msg.event {
            SessionEvent::SourceInfo(info) => Some(info),
            _ => None,
        },
        Err(_) => None,
    })
    .await;

    assert_eq!(info.address.as_deref(), Some(PEER), "which phone");
    assert_eq!(
        info.link.as_deref(),
        Some("aptX · 44.1 kHz · joint stereo"),
        "what was negotiated"
    );
    // Rendered for a human, both facts on one line.
    assert!(info.to_string().contains(PEER));
    assert!(info.to_string().contains("aptX"));
}

#[tokio::test]
async fn an_sco_request_is_refused_without_disturbing_the_acl_link() {
    // A phone that also wants a headset link must be told no, cleanly.
    let (transport, _rx) = connected().await;
    let addr: BdAddr = "11:22:33:44:55:66".parse().unwrap();
    let mut params = addr.to_wire().to_vec();
    params.extend_from_slice(&[0x0C, 0x02, 0x5A]);
    params.push(0x00); // SCO
    transport.push(HciPacket::Event {
        code: code::CONNECTION_REQUEST,
        params: Bytes::from(params),
    });
    eventually("sco refused", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::REJECT_CONNECTION_REQUEST)
            .then_some(())
    })
    .await;
}
