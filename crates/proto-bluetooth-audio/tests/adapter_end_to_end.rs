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
use castaway_test_support::eventually;
use proto_bluetooth_audio::adapter::{BluetoothAdapter, BluetoothConfig};
use proto_bluetooth_audio::avctp::CommandResponse;
use proto_bluetooth_audio::avdtp::{Message, Seid, Signal};
use proto_bluetooth_audio::codec::{ChannelModes, CodecCapability, SampleRates};
use proto_bluetooth_audio::obex::{ChosenImage, Header as ObexHeader, ObexPacket};
use substrate_hci::{
    event::code, AclPacket, BdAddr, ConnectionHandle, HciPacket, HciTransport, PacketBoundary,
    ScriptedTransport, Status,
};
use substrate_l2cap::ertm::Segmentation;
use substrate_l2cap::{
    ChannelMode, Cid, FcsType, Frame, L2capPdu, Psm, RetransmissionConfig, Signal as L2capSignal,
};
use substrate_sdp::{DataElement, SdpServer, ServiceRecord, Uuid};
use tokio::sync::mpsc;

const PEER: &str = "AA:BB:CC:DD:EE:FF";
const HANDLE: u16 = 0x000B;

/// A second phone, and the link the scripted controller gives it.
///
/// Two of them is not an exotic case — it is a room with two people in it, which is the
/// room this panel lives in.
const PEER2: &str = "99:88:77:66:55:44";
const HANDLE2: u16 = 0x000C;

/// The ACL handle the scripted controller hands a peer.
///
/// Derived from the address rather than remembered, so the responder stays a pure
/// function of the command it is answering — and pinned, so the first phone keeps the
/// handle every test above names.
///
/// This is what stopped the two-phone case from being written before: the responder
/// answered *every* `ACCEPT_CONNECTION_REQUEST` with `PEER` on `HANDLE`, whatever address
/// had actually paged us. A second phone's link therefore arrived as a duplicate of the
/// first, its AVDTP landed on a link belonging to someone else, and the symptom was the
/// adapter appearing not to answer.
fn handle_for(addr: BdAddr) -> u16 {
    if addr == PEER2.parse().unwrap() {
        HANDLE2
    } else {
        HANDLE
    }
}

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
/// unpaced writer looks like it works (#71).
fn controller(acl_packets: u16, report_completions: bool) -> Arc<ScriptedTransport> {
    Arc::new(
        ScriptedTransport::new()
            .with_responder(move |sent| respond(acl_packets, report_completions, sent)),
    )
}

/// What that controller answers with, as a plain function so a test can wrap it.
fn respond(acl_packets: u16, report_completions: bool, sent: &HciPacket) -> Vec<HciPacket> {
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
    let HciPacket::Command {
        opcode,
        params: cmd,
    } = sent
    else {
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
            // Command status, then the link coming up — for the address that actually
            // paged us, on a handle of its own. Answering with a fixed peer is what a
            // controller with one phone in the world would do, and it is invisible until
            // there are two (see `handle_for`).
            let addr = <[u8; 6]>::try_from(&cmd[..6])
                .map(BdAddr::from_wire)
                .unwrap_or_else(|_| PEER.parse().unwrap());
            let mut complete = vec![Status::SUCCESS.0];
            complete.extend_from_slice(&handle_for(addr).to_le_bytes());
            complete.extend_from_slice(&addr.to_wire());
            complete.push(0x01); // ACL
            complete.push(0x00); // encryption off
                                 // Command Status is status, credits, opcode — *not* the Command Complete
                                 // layout `params` was built for. It used to be sent with that layout, so
                                 // it failed to parse and was dropped as an undecodable event: harmless
                                 // while nothing counted command credits, and a stall the moment anything
                                 // did (#90).
            let mut status = vec![Status::SUCCESS.0, 0x01];
            status.extend_from_slice(&opcode.raw().to_le_bytes());
            return vec![
                HciPacket::Event {
                    code: code::COMMAND_STATUS,
                    params: Bytes::from(status),
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
}

/// Every complete L2CAP PDU the adapter has written, reassembled from ACL fragments.
fn sent_pdus(transport: &ScriptedTransport) -> Vec<L2capPdu> {
    // One reassembler *per link*, as the adapter itself keeps. Sharing a single one across
    // handles silently mis-assembles the moment two phones are connected: fragments from
    // one link land in the other's partial PDU and neither ever completes, which reads as
    // "the adapter stopped answering" rather than as a bug in the test harness.
    let mut reassemblers: std::collections::HashMap<u16, substrate_hci::Reassembler> =
        std::collections::HashMap::new();
    let mut out = Vec::new();
    for packet in transport.sent() {
        if let HciPacket::Acl(acl) = packet {
            let reassembler = reassemblers.entry(acl.handle.raw()).or_default();
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
/// Push an L2CAP PDU on a specific ACL link. Kept parameterised because the helpers
/// above it are, and a test that needs a second link should not have to reinvent it.
fn push_pdu_on(transport: &ScriptedTransport, handle: u16, pdu: &L2capPdu) {
    transport.push(HciPacket::Acl(AclPacket::new(
        ConnectionHandle::new(handle).unwrap(),
        PacketBoundary::FirstFlushable,
        pdu.encode().unwrap(),
    )));
}

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
    connected_with(
        transport,
        BluetoothConfig {
            decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
            ..BluetoothConfig::default()
        },
    )
    .await
}

/// The same again, with the config the caller wants — for the paths that only exist
/// when a flag is on.
async fn connected_with(
    transport: Arc<ScriptedTransport>,
    config: BluetoothConfig,
) -> (Arc<ScriptedTransport>, mpsc::Receiver<SourceMessage>) {
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(&transport) as Arc<dyn HciTransport>,
        config,
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
    open_channel_on(transport, HANDLE, psm, phone_cid).await
}

/// The same, on a nominated ACL link.
async fn open_channel_on(
    transport: &ScriptedTransport,
    handle: u16,
    psm: Psm,
    phone_cid: u16,
) -> (Cid, Cid) {
    let before = sent_pdus(transport).len();
    push_pdu_on(
        transport,
        handle,
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
    // Its configuration request has to be answered with *its* identifier: a response
    // carrying someone else's answers a proposal that may already have been withdrawn,
    // and the adapter is right to ignore it.
    let config_id = eventually("configuration request", || {
        sent_pdus(transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
            .flatten()
            .find_map(|sig| match sig {
                L2capSignal::ConfigurationRequest { id, .. } => Some(id),
                _ => None,
            })
    })
    .await;

    // Accept its configuration, and configure our own direction — on the link this
    // channel belongs to. These two used to go out on `HANDLE` whatever link the caller
    // named, which is invisible with one phone connected and fatal with two: L2CAP channel
    // ids are per-link, so a second phone's 0x0040 is a *different* channel that happens
    // to share a number. The configuration handshake landed on the first phone's channel,
    // the second one never reached Open, and every AVDTP signal it then sent was dropped —
    // which is what "the second link's signalling goes unanswered" in #95 was.
    push_pdu_on(
        transport,
        handle,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConfigurationResponse {
                id: config_id,
                source_cid: sink_cid,
                flags: 0,
                result: substrate_l2cap::ConfigResult::Success,
                options: vec![],
            }
            .encode()
            .unwrap(),
        ),
    );
    push_pdu_on(
        transport,
        handle,
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
    avdtp_on(transport, HANDLE, cid, transaction, signal, payload).await
}

/// The same, on a nominated ACL link.
async fn avdtp_on(
    transport: &ScriptedTransport,
    handle: u16,
    cid: Cid,
    transaction: u8,
    signal: Signal,
    payload: &[u8],
) -> Message {
    let before = sent_pdus(transport).len();
    push_pdu_on(
        transport,
        handle,
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
async fn a_restricted_codec_table_advertises_only_what_it_was_given() {
    // A sender takes the first endpoint it also supports, so narrowing the table is the
    // only way to make one negotiate a codec it would not have chosen — which is how the
    // SBC fallback gets exercised against a phone that would always prefer AAC.
    let adapter = BluetoothAdapter::new(
        transport() as Arc<dyn HciTransport>,
        BluetoothConfig {
            decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
            codecs: Some(vec![AudioCodec::Sbc]),
            ..BluetoothConfig::default()
        },
    );
    assert_eq!(adapter.advertised_codecs(), vec![AudioCodec::Sbc]);

    // …and the default still offers the full table, LDAC first.
    let all = BluetoothAdapter::new(
        transport() as Arc<dyn HciTransport>,
        BluetoothConfig {
            decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
            ..BluetoothConfig::default()
        },
    );
    let codecs = all.advertised_codecs();
    assert_eq!(codecs.first(), Some(&AudioCodec::Ldac));
    assert!(
        codecs.contains(&AudioCodec::Sbc),
        "SBC is the guaranteed floor"
    );
    assert!(codecs.len() > 1);
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

/// A controller that answers everything except one bring-up command, which it swallows.
///
/// The documented idle stall on this project's dongle, reduced to one lost completion.
fn controller_that_swallows(lost: substrate_hci::OpCode) -> Arc<ScriptedTransport> {
    Arc::new(ScriptedTransport::new().with_responder(move |sent| {
        if matches!(sent, HciPacket::Command { opcode, .. } if *opcode == lost) {
            return Vec::new();
        }
        respond(8, true, sent)
    }))
}

#[tokio::test(start_paused = true)]
async fn bring_up_survives_a_controller_that_swallows_a_completion() {
    // #90 at the seam it lives at. Bring-up is a queue advanced only by a completion, so
    // one lost answer stopped it dead: no `Ready`, no `WriteScanEnable`, and nothing in
    // the log — the panel came up, the UI looked healthy, and the receiver was simply
    // never discoverable over Bluetooth. The unit tests pin the watchdog; this pins the
    // actor actually *arming* it, which is the half that was missing (during bring-up
    // `links` is empty, so the loop had no deadline of any kind).
    let transport = controller_that_swallows(substrate_hci::OpCode::WRITE_LOCAL_NAME);
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(&transport) as Arc<dyn HciTransport>,
        BluetoothConfig::default(),
    ));
    let (tx, _rx) = mpsc::channel(16);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Bluetooth, "listener"), tx);
    tokio::spawn(Arc::clone(&adapter).run(sink));

    eventually("the swallowed command", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::WRITE_LOCAL_NAME)
            .then_some(())
    })
    .await;
    assert!(
        !transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::WRITE_SCAN_ENABLE),
        "bring-up is stopped on the unanswered command, which is the premise"
    );

    // Past the deadline. The clock is the runtime's, so this costs no real time.
    tokio::time::advance(Duration::from_secs(6)).await;
    eventually("bring-up finishing anyway", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::WRITE_SCAN_ENABLE)
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_phone_pairs_without_any_prompt() {
    // #68 end to end: the controller asks, the adapter answers, nobody is prompted.
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
    // #71, reproduced: a controller with two ACL buffers that has not yet freed either.
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
async fn the_sink_reports_its_delay_on_the_wire() {
    // #89 through the actor. The pure session decides to send it; this is the half that
    // was structurally missing — the adapter could only ever push a *reply*, so a sink
    // that wanted to originate a command had nowhere to put it. Everything below the
    // assert is a phone configuring a stream and asking, by naming the category, for the
    // number it needs to keep its video in sync.
    let (transport, _rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = discover
        .payload
        .chunks(2)
        .filter_map(|c| Seid::from_shifted(c[0]).ok())
        .nth(2)
        .expect("an aptX endpoint");
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
    assert!(
        proto_bluetooth_audio::avdtp::lists_category(
            &caps.payload,
            proto_bluetooth_audio::avdtp::category::DELAY_REPORTING
        ),
        "the capability is what makes the phone ask for the report"
    );

    // Narrowed to one rate and one channel mode, as a configuration must be.
    let codec = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    }
    .encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    set.push(0x08); // delay reporting: the phone will accept a DELAYREPORT
    set.push(0x00);

    let before = sent_pdus(&transport).len();
    let accept = avdtp(&transport, signaling, 3, Signal::SetConfiguration, &set).await;
    assert_eq!(
        accept.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "the endpoint advertised this configuration, so it must take it"
    );

    let report = eventually("a delay report", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| Message::decode(&pdu.payload).ok())
            .find(|m| m.signal == Signal::DelayReport)
    })
    .await;
    assert_eq!(
        report.message_type,
        proto_bluetooth_audio::avdtp::MessageType::Command,
        "SNK→SRC: this is us telling the phone, not answering it"
    );
    assert_eq!(report.payload[0], seid.shifted());
    let tenths = u16::from_be_bytes([report.payload[1], report.payload[2]]);
    assert_ne!(
        tenths, 0,
        "zero is what the phone assumed before this existed"
    );
    assert_eq!(
        u64::from(tenths) * 100,
        u64::try_from(proto_bluetooth_audio::sink::DEFAULT_SINK_DELAY.as_micros()).unwrap()
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
async fn a_pipeline_that_lets_go_gets_the_phone_paused_so_play_can_recover() {
    // The failure this ends, measured on the panel: the output device vanished (the
    // monitor slept, taking the HDMI endpoint with it), the session was torn down — and
    // the phone stayed connected, kept streaming, and pause/play could not bring the
    // audio back. Only a full disconnect/reconnect recovered it.
    //
    // The cause is a state-machine desync. Our AVDTP side is responder-only, so clearing
    // our sink state leaves the peer's stream STARTED; it keeps sending, we keep dropping,
    // and pause/play does nothing because the phone was never in a state that needed
    // re-STARTing. An AVRCP pause is the lever that makes the phone suspend, which is the
    // event that legitimately clears our side and lets the next play open a new stream.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;
    // AVRCP needs somewhere to go, or the pause is silently skipped.
    // The second element is the phone's channel id, which is what our outbound PDUs
    // are addressed to.
    let (_, avctp_peer) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = eventually("an aptX endpoint", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .nth(2)
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
    // Narrowed to one rate and one channel mode: a configuration that still carries the
    // whole advertised range is not a configuration, and is refused.
    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(&transport, signaling, 3, Signal::SetConfiguration, &set).await;
    avdtp(&transport, signaling, 4, Signal::Open, &[seid.shifted()]).await;
    let (media, _) = open_channel(&transport, Psm::AVDTP, 0x0041).await;
    avdtp(&transport, signaling, 5, Signal::Start, &[seid.shifted()]).await;

    let msg = eventually("an audio session event", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::Audio { .. }))
    })
    .await;
    let SessionEvent::Audio { source, .. } = msg.event else {
        panic!("expected an audio session");
    };
    let FrameSource::Encoded(frames) = source else {
        panic!("audio must arrive as encoded frames");
    };

    // The pipeline lets go — exactly what happened when WASAPI reported the endpoint
    // "no longer available" and the audio session thread returned.
    drop(frames);

    let before = sent_pdus(&transport).len();
    // The phone, knowing nothing about any of this, keeps sending.
    for _ in 0..4 {
        push_pdu(
            &transport,
            &L2capPdu::new(
                media,
                Bytes::copy_from_slice(&[0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]),
            ),
        );
    }

    // It must be told to stop, on the AVRCP channel, with a passthrough PAUSE.
    let paused = eventually("an avrcp pause", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            // 0x7C is the PASS_THROUGH opcode, 0x46 the PAUSE operand.
            .find(|pdu| {
                pdu.cid == avctp_peer && pdu.payload.windows(2).any(|w| w == [0x7C, 0x46])
            })
    })
    .await;
    assert_eq!(
        paused.cid, avctp_peer,
        "the pause must go out on the AVRCP channel"
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
    // have guessed — which is the whole of #70.
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
    let SessionEvent::Audio { source, format, .. } = msg.event else {
        panic!("expected an audio session, got {:?}", msg.event);
    };
    // The negotiated rate must reach the pipeline, not a default. aptX has no in-band
    // rate, so getting this wrong plays the stream ~9% slow and logs nothing (#70).
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
async fn a_media_channel_that_dies_without_signaling_still_ends_the_session() {
    // The dangling state #212 found: the media L2CAP channel closes with no AVDTP Close
    // ever arriving (a phone that crashes, a stack that skips the handshake), and the
    // adapter used to clear the frame sender while leaving the session marked open — a
    // session the manager held forever, for audio that could never resume.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = eventually("an aptX endpoint", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .nth(2)
    })
    .await;
    avdtp(
        &transport,
        signaling,
        2,
        Signal::GetAllCapabilities,
        &[seid.shifted()],
    )
    .await;
    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(&transport, signaling, 3, Signal::SetConfiguration, &set).await;
    avdtp(&transport, signaling, 4, Signal::Open, &[seid.shifted()]).await;
    let (media, _) = open_channel(&transport, Psm::AVDTP, 0x0041).await;
    avdtp(&transport, signaling, 5, Signal::Start, &[seid.shifted()]).await;

    eventually("an audio session event", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::Audio { .. }))
    })
    .await;

    // The phone tears the media channel down at the L2CAP layer and says nothing on the
    // signaling channel.
    push_pdu(
        &transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::DisconnectionRequest {
                id: 9,
                dest_cid: media,
                source_cid: Cid::new(0x0041),
            }
            .encode()
            .unwrap(),
        ),
    );

    // The session must end now — not whenever an AVDTP Close deigns to arrive.
    eventually("the session end", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::End))
    })
    .await;

    // A late Close from a stack that half-finishes the handshake finds the session
    // already ended: it is still answered, and it must not end the session twice.
    let reply = avdtp(&transport, signaling, 6, Signal::Close, &[seid.shifted()]).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "the late Close is still acknowledged"
    );
    while let Ok(msg) = rx.try_recv() {
        assert!(
            !matches!(msg.event, SessionEvent::End),
            "one dead transport must not end the session twice"
        );
    }
}

#[tokio::test]
async fn the_control_surface_survives_avctp_connecting_before_the_stream() {
    // Both orders happen and the sender chooses. A phone that opens AVCTP first — which
    // an iPhone does, nine seconds ahead of START in one capture — used to have its
    // control surface emitted while no session was active, rejected, and dropped. That
    // silently costs the panel every transport control it has over that phone.
    let (transport, mut rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    // AVCTP first, deliberately.
    let _ = open_channel(&transport, Psm::AVCTP, 0x0050).await;

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

    // The control surface must arrive once the session exists, not before it.
    let mut saw_audio = false;
    let mut saw_control = false;
    for _ in 0..40 {
        match rx.try_recv() {
            Ok(msg) => match msg.event {
                SessionEvent::Audio { .. } => saw_audio = true,
                SessionEvent::ControlSurface(_) => {
                    assert!(saw_audio, "control surface must not precede the session");
                    saw_control = true;
                }
                _ => {}
            },
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
        if saw_control {
            break;
        }
    }
    assert!(saw_audio, "the session should have started");
    assert!(saw_control, "the control surface was dropped");
}

/// Pull every AVRCP vendor PDU id the adapter has sent on `cid`.
fn sent_avrcp_pdus(transport: &ScriptedTransport) -> Vec<u8> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
        .filter_map(|frame| proto_bluetooth_audio::VendorPdu::parse(&frame.operands).ok())
        .map(|v| v.pdu_id)
        .collect()
}

#[tokio::test]
async fn the_adapter_subscribes_to_metadata_changes() {
    // Without this the now-playing card is a snapshot of the instant AVCTP connected:
    // skip a track and the screen keeps the old one forever, and the play state stays at
    // whatever `PlaybackState::default()` happens to be. One request is not a design.
    let (transport, _rx) = connected().await;
    let _ = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    let pdus = eventually("the avrcp opening traffic", || {
        let pdus = sent_avrcp_pdus(&transport);
        (pdus.len() >= 5).then_some(pdus)
    })
    .await;
    assert!(
        pdus.contains(&proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES),
        "metadata must be asked for up front: {pdus:x?}"
    );
    assert!(
        pdus.contains(&proto_bluetooth_audio::avrcp::pdu::GET_PLAY_STATUS),
        "duration comes from nowhere else — POS_CHANGED carries only a position: {pdus:x?}"
    );

    // The events themselves, not just how many: a count passes just as happily when the
    // wrong three are subscribed.
    let events = registered_events(&transport);
    for (event, what) in [
        (
            proto_bluetooth_audio::avrcp::event::PLAYBACK_STATUS_CHANGED,
            "play state",
        ),
        (
            proto_bluetooth_audio::avrcp::event::TRACK_CHANGED,
            "track changes",
        ),
        (
            proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED,
            "position",
        ),
    ] {
        assert!(
            events.iter().any(|(id, _)| *id == event),
            "{what} must be subscribed: {events:x?}"
        );
    }

    // The interval field is in seconds and only PLAYBACK_POS_CHANGED uses it. Zero there
    // would mean "never report", which is a scrubber that does not move.
    let (_, interval) = events
        .iter()
        .find(|(id, _)| *id == proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED)
        .expect("position is subscribed");
    assert!(
        *interval > 0,
        "a zero reporting interval never reports: {events:x?}"
    );
}

/// Every `(event id, reporting interval)` the adapter has registered for.
///
/// The parameters are always five bytes — one event id and a big-endian interval — even
/// for events that ignore the interval (BlueZ `AVRCP_REGISTER_NOTIFICATION_PARAM_LENGTH`).
fn registered_events(transport: &ScriptedTransport) -> Vec<(u8, u32)> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
        .filter_map(|frame| proto_bluetooth_audio::VendorPdu::parse(&frame.operands).ok())
        .filter(|v| v.pdu_id == proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION)
        .filter_map(|v| {
            let raw = v.parameters.get(1..5)?;
            Some((
                *v.parameters.first()?,
                u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            ))
        })
        .collect()
}

/// An AVRCP notification response, as the phone would send it.
fn notification(
    transaction: u8,
    ctype: proto_bluetooth_audio::Ctype,
    event: u8,
    body: &[u8],
) -> Bytes {
    let mut params = BytesMut::new();
    params.put_u8(event);
    params.extend_from_slice(body);
    let frame = proto_bluetooth_audio::avrcp::vendor_command(
        ctype,
        proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION,
        &params,
    );
    proto_bluetooth_audio::AvctpMessage::command(transaction, frame.encode()).encode()
}

#[tokio::test]
async fn a_changed_notification_is_re_registered_and_acted_on() {
    // AVRCP notifications are one-shot: CHANGED ends the subscription. A stack that does
    // not re-register hears about exactly one track change and then goes quiet, which
    // looks like it works right up until the second song.
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    eventually("the subscriptions", || {
        (sent_avrcp_pdus(&transport).len() >= 3).then_some(())
    })
    .await;

    let before = sent_avrcp_pdus(&transport).len();
    // The phone reports the track changed, carrying only a track id.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            notification(
                1,
                proto_bluetooth_audio::Ctype::Changed,
                proto_bluetooth_audio::avrcp::event::TRACK_CHANGED,
                &[0u8; 8],
            ),
        ),
    );

    let after = eventually("the response to a track change", || {
        let pdus = sent_avrcp_pdus(&transport);
        (pdus.len() > before).then_some(pdus)
    })
    .await;
    let new = &after[before..];
    assert!(
        new.contains(&proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION),
        "the subscription must be renewed or this is the last change we ever hear: {new:x?}"
    );
    assert!(
        new.contains(&proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES),
        "a track change carries only an id, so the metadata must be re-read: {new:x?}"
    );
}

/// A `GetElementAttributes` response, as a phone would send it.
fn attributes_response(items: &[(u32, &[u8])]) -> Bytes {
    let mut attrs = BytesMut::new();
    attrs.put_u8(u8::try_from(items.len()).unwrap());
    for (id, value) in items {
        attrs.put_u32(*id);
        attrs.put_u16(0x006A); // UTF-8
        attrs.put_u16(u16::try_from(value.len()).unwrap());
        attrs.extend_from_slice(value);
    }
    let frame = proto_bluetooth_audio::avrcp::vendor_command(
        proto_bluetooth_audio::Ctype::Stable,
        proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES,
        &attrs,
    );
    proto_bluetooth_audio::AvctpMessage::command(0, frame.encode()).encode()
}

/// Every signalling command the adapter has sent since `before`.
fn signals_after(transport: &ScriptedTransport, before: usize) -> Vec<L2capSignal> {
    sent_pdus(transport)
        .into_iter()
        .skip(before)
        .filter(|pdu| pdu.cid == Cid::SIGNALING)
        .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
        .flatten()
        .collect()
}

/// Accept an L2CAP connection the adapter opened *outwards*, in `mode`.
///
/// The other half of `open_channel`: the cover-art chain is the only place the receiver
/// dials rather than answers, so the harness has to be able to play the responder.
/// Returns the identifier the adapter allocated for the channel.
async fn accept_outgoing(
    transport: &ScriptedTransport,
    psm: Psm,
    phone_cid: u16,
    mode: ChannelMode,
) -> Cid {
    let before = sent_pdus(transport).len();
    let (id, adapter_cid) = eventually("an outgoing connection request", || {
        signals_after(transport, 0)
            .into_iter()
            .find_map(|sig| match sig {
                L2capSignal::ConnectionRequest {
                    id,
                    psm: p,
                    source_cid,
                } if p == psm => Some((id, source_cid)),
                _ => None,
            })
    })
    .await;

    push_pdu(
        transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConnectionResponse {
                id,
                dest_cid: Cid::new(phone_cid),
                source_cid: adapter_cid,
                result: substrate_l2cap::ConnectionResult::Success,
                status: 0,
            }
            .encode()
            .unwrap(),
        ),
    );

    // Its configuration request names the mode it wants; the response has to agree, or
    // the adapter — which is the dialling side, and therefore the one that yields — comes
    // back down to whatever we asked for instead.
    let config_id = eventually("its configuration request", || {
        signals_after(transport, before)
            .into_iter()
            .find_map(|sig| match sig {
                L2capSignal::ConfigurationRequest { id, dest_cid, .. }
                    if dest_cid == Cid::new(phone_cid) =>
                {
                    Some(id)
                }
                _ => None,
            })
    })
    .await;

    let mut options = vec![substrate_l2cap::ConfigOption::Mtu(672)];
    if mode == ChannelMode::EnhancedRetransmission {
        options.push(substrate_l2cap::ConfigOption::Retransmission(
            RetransmissionConfig::ertm(600),
        ));
        options.push(substrate_l2cap::ConfigOption::Fcs(FcsType::Crc16));
    }
    push_pdu(
        transport,
        &L2capPdu::new(
            Cid::SIGNALING,
            L2capSignal::ConfigurationResponse {
                id: config_id,
                source_cid: adapter_cid,
                flags: 0,
                result: substrate_l2cap::ConfigResult::Success,
                options: options.clone(),
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
                id: 0x5A,
                dest_cid: adapter_cid,
                flags: 0,
                options,
            }
            .encode()
            .unwrap(),
        ),
    );
    adapter_cid
}

/// Wrap an SDU as an ERTM I-frame addressed to the adapter's channel.
fn ertm_pdu(adapter_cid: Cid, tx_seq: u8, req_seq: u8, payload: &[u8]) -> L2capPdu {
    L2capPdu::new(
        adapter_cid,
        Frame::Information {
            tx_seq,
            req_seq,
            final_bit: false,
            sar: Segmentation::Unsegmented,
            sdu_len: None,
            payload: Bytes::copy_from_slice(payload),
        }
        .encode(adapter_cid, FcsType::Crc16),
    )
}

/// Every SDU the adapter has sent on an ERTM channel, in order.
///
/// Addressed — and checksummed — with the *phone's* identifier for the channel, because
/// that is the one the adapter puts on a PDU it sends. Decoding with our own is the same
/// CID mix-up as ever, and here it shows up as every frame failing its checksum.
fn ertm_sdus(transport: &ScriptedTransport, phone_cid: Cid) -> Vec<Bytes> {
    sent_pdus(transport)
        .into_iter()
        .filter(|pdu| pdu.cid == phone_cid)
        .filter_map(|pdu| Frame::decode(&pdu.payload, phone_cid, FcsType::Crc16).ok())
        .filter_map(|frame| match frame {
            Frame::Information { payload, .. } => Some(payload),
            _ => None,
        })
        .collect()
}

/// Which attribute ids a `GetElementAttributes` command asked for.
fn requested_attributes(body: &[u8]) -> Option<Vec<u32>> {
    let msg = proto_bluetooth_audio::AvctpMessage::decode(body).ok()?;
    let frame = proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok()?;
    let vendor = proto_bluetooth_audio::VendorPdu::parse(&frame.operands).ok()?;
    if vendor.pdu_id != proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES {
        return None;
    }
    // Eight bytes of track identifier, a count, then four bytes per attribute.
    let count = usize::from(*vendor.parameters.get(8)?);
    Some(
        (0..count)
            .filter_map(|i| {
                let at = 9 + i * 4;
                let bytes = vendor.parameters.get(at..at + 4)?;
                Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            })
            .collect(),
    )
}

#[tokio::test]
async fn an_inbound_metadata_request_is_answered_and_does_not_empty_the_card() {
    // A head unit asking *us* what is playing. Real GM and Hyundai-Kia units enumerate
    // attributes 1..=8 unconditionally, and the request used to fall into the branch that
    // parses responses — where its eight-byte track identifier reads as an attribute
    // count of zero and wipes the now-playing card the phone had just filled in.
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    eventually("the avrcp opening traffic", || {
        (sent_avrcp_pdus(&transport).len() >= 3).then_some(())
    })
    .await;

    // The phone tells us what is playing.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            attributes_response(&[
                (proto_bluetooth_audio::avrcp::attribute::TITLE, b"Derezzed"),
                (
                    proto_bluetooth_audio::avrcp::attribute::ARTIST,
                    b"Daft Punk",
                ),
            ]),
        ),
    );

    // Then something else asks us the same question, attribute 8 included.
    let before = sent_pdus(&transport).len();
    let request = proto_bluetooth_audio::avrcp::get_element_attributes(
        &proto_bluetooth_audio::avrcp::attribute::ALL,
    );
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            proto_bluetooth_audio::AvctpMessage::command(3, request.encode()).encode(),
        ),
    );

    let answer = eventually("an answer to the inbound request", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
            .filter(|msg| msg.cr == CommandResponse::Response)
            .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
            .find(|frame| frame.ctype == proto_bluetooth_audio::Ctype::Stable)
    })
    .await;

    let vendor = proto_bluetooth_audio::VendorPdu::parse(&answer.operands).unwrap();
    assert_eq!(
        vendor.pdu_id,
        proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES
    );
    let parsed =
        proto_bluetooth_audio::avrcp::parse_element_attributes(&vendor.parameters).unwrap();
    assert_eq!(
        parsed.now_playing.title.as_deref(),
        Some("Derezzed"),
        "attribute 8 in the request must not cost us the seven we can answer"
    );
    assert_eq!(parsed.now_playing.artist.as_deref(), Some("Daft Punk"));
}

#[tokio::test]
async fn the_image_server_is_connected_before_the_handle_is_asked_for() {
    // The ordering #74 turned on. AOSP's Target strips attribute 8 from a metadata
    // response whenever no cover-art client is connected, so a receiver that waits to see
    // a handle before connecting waits forever — which is exactly what an iPhone streaming
    // happily and never sending a handle looked like.
    let (transport, _rx) = connected().await;
    let _ = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    let first = eventually("the opening metadata request", || {
        sent_pdus(&transport)
            .into_iter()
            .find_map(|pdu| requested_attributes(&pdu.payload))
    })
    .await;
    assert!(
        !first.contains(&proto_bluetooth_audio::avrcp::attribute::COVER_ART_HANDLE),
        "asking for attribute 8 with no BIP session gets it stripped: {first:?}"
    );
    assert!(
        first.contains(&proto_bluetooth_audio::avrcp::attribute::TITLE),
        "the text is worth having immediately, without waiting on OBEX"
    );

    // …and the search for the image server is already under way, on its own initiative
    // rather than in response to a handle we were never going to be sent.
    eventually("an outgoing sdp connection request", || {
        signals_after(&transport, 0)
            .into_iter()
            .find_map(|sig| match sig {
                L2capSignal::ConnectionRequest {
                    psm, source_cid, ..
                } if psm == Psm::SDP => Some(source_cid),
                _ => None,
            })
    })
    .await;
}

#[tokio::test]
async fn cover_art_is_discovered_connected_and_fetched() {
    // The whole chain, end to end and with no radio: SDP finds the image server, the
    // channel comes up in Enhanced Retransmission Mode because GOEP 2.0 says so, OBEX
    // connects, *then* attribute 8 is asked for, and the JPEG that comes back reaches the
    // now-playing card.
    let (transport, mut rx) = connected_with(
        transport(),
        BluetoothConfig {
            decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
            // …and on past the thumbnail, into the properties listing and the larger form
            // it may name (#75).
            ..BluetoothConfig::default()
        },
    )
    .await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    // A session, so the metadata events are actually delivered rather than dropped for a
    // source the manager does not consider active.
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;
    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = Seid::from_shifted(discover.payload[4]).unwrap();
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

    // 1. It goes looking for the image server over SDP, unprompted.
    let sdp_phone = Cid::new(0x0060);
    let sdp_cid = accept_outgoing(&transport, Psm::SDP, sdp_phone.raw(), ChannelMode::Basic).await;

    // 2. …and we answer with an AVRCP Target record that publishes one, alongside a
    //    browsing channel — the shape that used to send the fetch to the wrong PSM.
    let record = ServiceRecord::new()
        .with(
            substrate_sdp::record::attr::SERVICE_RECORD_HANDLE,
            DataElement::Uint(0x10000),
        )
        .with(
            substrate_sdp::record::attr::SERVICE_CLASS_ID_LIST,
            DataElement::uuid_seq([Uuid::AV_REMOTE_CONTROL_TARGET]),
        )
        .with(
            substrate_sdp::record::attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![
                DataElement::Sequence(vec![
                    DataElement::Sequence(vec![
                        DataElement::Uuid(Uuid::L2CAP),
                        DataElement::Uint(0x001B),
                    ]),
                    DataElement::Sequence(vec![
                        DataElement::Uuid(Uuid::AVCTP),
                        DataElement::Uint16(0x0104),
                    ]),
                ]),
                DataElement::Sequence(vec![
                    DataElement::Sequence(vec![
                        DataElement::Uuid(Uuid::L2CAP),
                        DataElement::Uint(0x1005),
                    ]),
                    DataElement::Sequence(vec![DataElement::Uuid(Uuid::OBEX)]),
                ]),
            ]),
        );
    let phone_sdp = SdpServer::new().with(record);
    let query = eventually("the sdp query", || {
        sent_pdus(&transport)
            .into_iter()
            .find(|pdu| pdu.cid == sdp_phone)
            .map(|pdu| pdu.payload)
    })
    .await;
    push_pdu(
        &transport,
        &L2capPdu::new(sdp_cid, phone_sdp.handle(&query)),
    );

    // 3. The image channel opens — and it must be ERTM, or a GOEP 2.0 responder refuses.
    let art_phone = Cid::new(0x0061);
    let art_cid = accept_outgoing(
        &transport,
        Psm::new(0x1005).unwrap(),
        art_phone.raw(),
        ChannelMode::EnhancedRetransmission,
    )
    .await;

    // 4. OBEX CONNECT, carried as an ERTM I-frame with its frame check sequence.
    let connect = eventually("an obex connect", || {
        ertm_sdus(&transport, art_phone).into_iter().next()
    })
    .await;
    assert_eq!(connect[0], 0x80, "OBEX CONNECT: {connect:02x?}");
    let connect_reply = ObexPacket {
        code: 0xA0,
        prefix: Bytes::copy_from_slice(&[0x10, 0x00, 0x04, 0x00]),
        headers: vec![ObexHeader::ConnectionId(7)],
    }
    .encode();
    push_pdu(&transport, &ertm_pdu(art_cid, 0, 1, &connect_reply));

    // 5. *Now* the metadata is re-read, this time asking for the image handle.
    let asked = eventually("a metadata request that includes attribute 8", || {
        sent_pdus(&transport)
            .into_iter()
            .filter_map(|pdu| requested_attributes(&pdu.payload))
            .find(|ids| ids.contains(&proto_bluetooth_audio::avrcp::attribute::COVER_ART_HANDLE))
    })
    .await;
    assert_eq!(asked.len(), 8, "all of it, handle included: {asked:?}");

    // 6. The phone answers with a handle, and the image is pulled over the same session.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            attributes_response(&[
                (proto_bluetooth_audio::avrcp::attribute::TITLE, b"Derezzed"),
                (
                    proto_bluetooth_audio::avrcp::attribute::COVER_ART_HANDLE,
                    b"0000001",
                ),
            ]),
        ),
    );

    let get = eventually("an obex get for the handle", || {
        ertm_sdus(&transport, art_phone)
            .into_iter()
            .find(|sdu| sdu.first() == Some(&0x83))
    })
    .await;
    let parsed = ObexPacket::decode(&get, 0).unwrap();
    assert!(
        parsed
            .headers
            .contains(&ObexHeader::ImageHandle("0000001".into())),
        "the handle goes in Img-Handle, which is what a responder matches on: {:?}",
        parsed.headers
    );
    assert!(
        parsed
            .headers
            .contains(&ObexHeader::Type("x-bt/img-thm".into())),
        "and the abbreviated BIP type: {:?}",
        parsed.headers
    );

    // 7. The thumbnail comes back, and lands on the card.
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    jpeg.resize(64, 0x5A);
    let image = ObexPacket {
        code: 0xA0,
        prefix: Bytes::new(),
        headers: vec![ObexHeader::EndOfBody(Bytes::from(jpeg.clone()))],
    }
    .encode();
    push_pdu(&transport, &ertm_pdu(art_cid, 1, 1, &image));

    let artwork = eventually("artwork on the now-playing card", || match rx.try_recv() {
        Ok(msg) => match msg.event {
            SessionEvent::NowPlaying(now) => now.artwork,
            _ => None,
        },
        Err(_) => None,
    })
    .await;
    assert_eq!(&artwork.data[..], &jpeg[..]);

    // 8. With the probe on, the session then asks what other forms exist — the open half
    //    of #75. Once per track, on the same session, a GET for a different MIME type.
    let props_get = eventually("an obex get for the properties", || {
        ertm_sdus(&transport, art_phone).into_iter().find(|sdu| {
            ObexPacket::decode(sdu, 0).is_ok_and(|p| {
                p.headers
                    .contains(&ObexHeader::Type("x-bt/img-properties".into()))
            })
        })
    })
    .await;
    assert!(ObexPacket::decode(&props_get, 0)
        .unwrap()
        .headers
        .contains(&ObexHeader::ImageHandle("0000001".into())));

    // 9. The phone lists a form larger than the linked thumbnail's fixed 200x200, and
    //    within the airtime ceiling…
    let listing = br#"<image-properties version="1.0" handle="0000001">
<native encoding="JPEG" pixel="200*200" />
<variant encoding="JPEG" pixel="480*480" />
</image-properties>"#;
    let props_reply = ObexPacket {
        code: 0xA0,
        prefix: Bytes::new(),
        headers: vec![ObexHeader::EndOfBody(Bytes::from_static(listing))],
    }
    .encode();
    push_pdu(&transport, &ertm_pdu(art_cid, 2, 1, &props_reply));

    // 10. …so it is fetched, with a descriptor naming the peer's own spelling of it. A
    //     size we invented would be refused: BIP compares these as strings.
    let image_get = eventually("an obex get for the larger form", || {
        ertm_sdus(&transport, art_phone).into_iter().find(|sdu| {
            ObexPacket::decode(sdu, 0)
                .is_ok_and(|p| p.headers.contains(&ObexHeader::Type("x-bt/img-img".into())))
        })
    })
    .await;
    let parsed = ObexPacket::decode(&image_get, 0).unwrap();
    assert!(
        parsed.headers.iter().any(|h| matches!(
            h,
            ObexHeader::ImageDescription(d)
                if d.contains(r#"pixel="480*480""#) && d.contains(r#"encoding="JPEG""#)
        )),
        "the descriptor must name the listed form: {:?}",
        parsed.headers
    );
    assert!(parsed
        .headers
        .contains(&ObexHeader::ImageHandle("0000001".into())));
}

#[tokio::test]
async fn a_form_too_large_for_the_radio_is_declined_in_favour_of_the_thumbnail() {
    // The ceiling is about airtime, not about the panel: this link is already carrying
    // the audio the picture belongs to, and several seconds of contended radio spent on
    // decoration is a dropout in the thing the decoration is for. A peer offering
    // something enormous gets the thumbnail we already have.
    use proto_bluetooth_audio::obex::ImageProperties;
    let huge = br#"<image-properties version="1.0" handle="1">
<native encoding="JPEG" pixel="200*200" />
<variant encoding="JPEG" pixel="2048*2048" />
</image-properties>"#;
    let props = ImageProperties::parse(huge).unwrap();
    assert_eq!(
        props.largest_decodable().map(ChosenImage::size),
        Some((2048, 2048)),
        "the offer itself is unbounded"
    );
    // 512 is the adapter's ceiling; mirrored here because the constant is private.
    assert_eq!(
        props
            .largest_decodable_within(512, u64::MAX)
            .map(ChosenImage::size),
        Some((200, 200)),
        "so we fall back to the native, which is the thumbnail we already fetched"
    );

    // A declared `maxsize` is honoured too, when the descriptor states one.
    let heavy = br#"<image-properties version="1.0" handle="1">
<native encoding="JPEG" pixel="200*200" />
<variant encoding="JPEG" pixel="480*480" maxsize="9000000" />
</image-properties>"#;
    let props = ImageProperties::parse(heavy).unwrap();
    assert_eq!(
        props
            .largest_decodable_within(512, 1024 * 1024)
            .map(ChosenImage::size),
        Some((200, 200)),
        "nine megabytes of cover art is not worth the radio"
    );
}

#[tokio::test]
async fn a_ranged_offer_over_the_ceiling_is_clamped_into_rather_than_declined() {
    // The other side of the ceiling, and the bug #245 reported. A `<variant>` carrying a
    // pixel *range* is an offer to transcode into anything inside it, so a ceiling above
    // ours bounds the *request*; judging the offer by that ceiling threw away the only
    // forms worth having and left the first Android phone on the bench fetching its
    // 200×200 native — fewer pixels than an iPhone's 280.
    //
    // Reconstructed from the bench log of 2026-08-08, not a capture.
    use proto_bluetooth_audio::obex::{CoverArtSession, ImageProperties};
    let android = br#"<image-properties version="1.0" handle="7161797">
<native encoding="JPEG" pixel="200*200" size="160000"/>
<variant encoding="JPEG" pixel="100*100-1280*1080"/>
<variant encoding="PNG" pixel="100*100-1280*1080"/>
</image-properties>"#;
    let props = ImageProperties::parse(android).unwrap();
    // 512 is the adapter's ceiling; mirrored here because the constant is private.
    let chosen = props
        .largest_decodable_within(512, 1024 * 1024)
        .expect("a range whose floor is under the ceiling has a size inside it");
    assert_eq!(chosen.size(), (512, 512), "clamped into, not dropped for");

    // …and what goes on the wire names that one size. Spelling the range back would ask
    // for a form no responder holds.
    let mut session = CoverArtSession::new(0x400);
    session
        .feed(
            &ObexPacket {
                code: 0xA0,
                prefix: Bytes::from_static(&[0x10, 0x00, 0x04, 0x00]),
                headers: vec![ObexHeader::ConnectionId(1)],
            }
            .encode(),
        )
        .unwrap();
    assert!(session.fetch_image("7161797", chosen));
    let get = ObexPacket::decode(&session.next_request().expect("a GET"), 0).unwrap();
    assert!(
        get.headers.iter().any(|h| matches!(
            h,
            ObexHeader::ImageDescription(d)
                if d.contains(r#"pixel="512*512""#) && d.contains(r#"encoding="JPEG""#)
        )),
        "one concrete size, in the peer's own encoding token: {:?}",
        get.headers
    );
}

#[tokio::test]
async fn a_peer_offering_nothing_bigger_than_the_thumbnail_is_not_asked_twice() {
    // The other half of the #75 gate. An iPhone's real listing tops out at 280x280 for a
    // 200x200 native, but a peer whose largest form *is* the thumbnail should cost no
    // second fetch at all — the upgrade exists to find a bigger picture, not to re-fetch
    // the one we have.
    use proto_bluetooth_audio::obex::ImageProperties;
    let same = br#"<image-properties version="1.0" handle="1">
<native encoding="JPEG" pixel="200*200" />
</image-properties>"#;
    let props = ImageProperties::parse(same).unwrap();
    assert_eq!(
        props.largest_decodable().map(ChosenImage::size),
        Some((200, 200)),
        "nothing on offer beyond what a thumbnail fetch already returns"
    );
}

#[tokio::test]
async fn we_open_avctp_ourselves_when_the_peer_does_not() {
    // Android opens AVCTP; an iPhone streams happily and never does. We are the AVRCP
    // Controller — the end that wants metadata and sends transport commands — so waiting
    // to be connected to left the now-playing card permanently empty on the phones people
    // are most likely to walk up with.
    let (transport, _rx) = connected().await;
    let (signaling, _) = open_channel(&transport, Psm::AVDTP, 0x0040).await;

    let discover = avdtp(&transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = Seid::from_shifted(discover.payload[4]).unwrap();
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
    // Deliberately never open AVCTP from the phone side.
    avdtp(&transport, signaling, 4, Signal::Start, &[seid.shifted()]).await;

    eventually("an outgoing avctp connection request", || {
        sent_pdus(&transport)
            .into_iter()
            .filter(|pdu| pdu.cid == Cid::SIGNALING)
            .filter_map(|pdu| L2capSignal::decode_all(&pdu.payload).ok())
            .flatten()
            .find_map(|sig| match sig {
                L2capSignal::ConnectionRequest { psm, .. } if psm == Psm::AVCTP => Some(()),
                _ => None,
            })
    })
    .await;
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
        // aptX is constant-rate: 44100 x 2 x 4 bits per sample.
        Some("aptX · 44.1 kHz · joint stereo · 352 kbps"),
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

/// The parameter block of a `GetElementAttributes` response, as a Target builds it.
fn attribute_params(items: &[(u32, &[u8])]) -> BytesMut {
    let mut attrs = BytesMut::new();
    attrs.put_u8(u8::try_from(items.len()).unwrap());
    for (id, value) in items {
        attrs.put_u32(*id);
        attrs.put_u16(0x006A); // UTF-8
        attrs.put_u16(u16::try_from(value.len()).unwrap());
        attrs.extend_from_slice(value);
    }
    attrs
}

/// One fragment of a vendor-dependent response.
///
/// Hand-built rather than going through `vendor_command`, which always writes packet type
/// 0 (single). The layout is BlueZ's `struct avrcp_header`: three company-id bytes, the
/// PDU id, the packet type in the low two bits of byte 4, then a big-endian length.
fn vendor_fragment(pdu_id: u8, packet_type: u8, params: &[u8]) -> Bytes {
    let mut operands = BytesMut::new();
    operands.put_u8(0x00);
    operands.put_u8(0x19);
    operands.put_u8(0x58); // BT SIG company id
    operands.put_u8(pdu_id);
    operands.put_u8(packet_type & 0b11);
    operands.put_u16(u16::try_from(params.len()).unwrap());
    operands.extend_from_slice(params);
    let frame = proto_bluetooth_audio::AvcFrame::panel(
        proto_bluetooth_audio::Ctype::Stable,
        0x00, // VENDOR DEPENDENT
        operands.freeze(),
    );
    proto_bluetooth_audio::AvctpMessage::command(0, frame.encode()).encode()
}

/// The parameter byte of any `REQUEST_CONTINUING_RESPONSE` the adapter has sent.
fn continuation_requests(transport: &ScriptedTransport) -> Vec<u8> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
        .filter_map(|frame| proto_bluetooth_audio::VendorPdu::parse(&frame.operands).ok())
        .filter(|v| v.pdu_id == proto_bluetooth_audio::avrcp::pdu::REQUEST_CONTINUING_RESPONSE)
        .filter_map(|v| v.parameters.first().copied())
        .collect()
}

#[tokio::test]
async fn a_fragmented_metadata_response_is_reassembled_rather_than_dropped() {
    // AV/C fixes the packet ceiling at 512 bytes (BlueZ `AVC_MTU`, avctp.h), so a metadata
    // response fragments on its own terms however large the L2CAP MTU is — a long or CJK
    // title, or simply all seven text attributes, is enough. Nothing used to read the
    // packet-type field, so the first fragment was parsed as the whole response, came back
    // `Truncated`, and was dropped by an `if let Ok(..)`: the card stayed blank for that
    // track, with nothing at any log level.
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    eventually("the avrcp opening traffic", || {
        (sent_avrcp_pdus(&transport).len() >= 3).then_some(())
    })
    .await;

    let params = attribute_params(&[
        (
            proto_bluetooth_audio::avrcp::attribute::TITLE,
            "整いました".as_bytes(),
        ),
        (
            proto_bluetooth_audio::avrcp::attribute::ARTIST,
            "DEMONDICE".as_bytes(),
        ),
    ]);
    // Split mid-value, which is what a Target does when the remaining room runs out
    // partway through an attribute — and the case a naive parser cannot survive.
    let split = params.len() / 2;
    let pdu = proto_bluetooth_audio::avrcp::pdu::GET_ELEMENT_ATTRIBUTES;

    let before = continuation_requests(&transport).len();
    push_pdu(
        &transport,
        &L2capPdu::new(avctp, vendor_fragment(pdu, 1, &params[..split])),
    );

    // The peer is holding the rest and will not send it unasked.
    let asked = eventually("a request for the next fragment", || {
        let seen = continuation_requests(&transport);
        (seen.len() > before).then(|| seen[before])
    })
    .await;
    assert_eq!(
        asked, pdu,
        "the continuation request names the *original* pdu id, which is what the \
         Target matches on (BlueZ avrcp_handle_request_continuing)"
    );

    // The remainder, labelled with the original pdu id — again as BlueZ's Target does
    // (`pdu->pdu_id = pending->pdu_id`), not with 0x40.
    push_pdu(
        &transport,
        &L2capPdu::new(avctp, vendor_fragment(pdu, 3, &params[split..])),
    );

    // Ask the adapter back what is playing: its answer is built from the metadata it
    // just reassembled, so a title here proves the halves were joined *before* parsing.
    // (Asserting on a `NowPlaying` event instead would need a live audio session, which
    // this test deliberately does not have — the bug is in the metadata path, not the
    // media one.)
    let before = sent_pdus(&transport).len();
    let request = proto_bluetooth_audio::avrcp::get_element_attributes(
        &proto_bluetooth_audio::avrcp::attribute::TEXT,
    );
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            proto_bluetooth_audio::AvctpMessage::command(5, request.encode()).encode(),
        ),
    );
    let answer = eventually("our answer to what is playing", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
            .filter(|msg| msg.cr == CommandResponse::Response)
            .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
            .find(|frame| frame.ctype == proto_bluetooth_audio::Ctype::Stable)
    })
    .await;
    let vendor = proto_bluetooth_audio::VendorPdu::parse(&answer.operands).unwrap();
    let parsed =
        proto_bluetooth_audio::avrcp::parse_element_attributes(&vendor.parameters).unwrap();
    assert_eq!(
        parsed.now_playing.title.as_deref(),
        Some("整いました"),
        "the halves must be joined before they are parsed"
    );
    assert_eq!(parsed.now_playing.artist.as_deref(), Some("DEMONDICE"));
}

#[tokio::test]
async fn position_reaches_the_card_and_not_applicable_is_not_shown_as_49_days() {
    // The scrubber sat at zero for the whole track: `event::PLAYBACK_POS_CHANGED` and
    // `get_play_status()` were both defined and referenced from nowhere, so duration
    // arrived (attribute 7) and position never did.
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    eventually("the avrcp opening traffic", || {
        (sent_avrcp_pdus(&transport).len() >= 5).then_some(())
    })
    .await;

    // 0xFFFFFFFF is the spec's "not applicable" — a live stream, or a player that does
    // not know — and rendering it literally is a track 49 days long.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            notification(
                7,
                proto_bluetooth_audio::Ctype::Changed,
                proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED,
                &u32::MAX.to_be_bytes(),
            ),
        ),
    );
    // Then a real one: 1500 ms into the track.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            notification(
                8,
                proto_bluetooth_audio::Ctype::Changed,
                proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED,
                &1500_u32.to_be_bytes(),
            ),
        ),
    );

    // Ask ourselves what is playing: the answer is built from what we recorded.
    let before = sent_pdus(&transport).len();
    let request = proto_bluetooth_audio::avrcp::get_element_attributes(
        &proto_bluetooth_audio::avrcp::attribute::TEXT,
    );
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            proto_bluetooth_audio::AvctpMessage::command(9, request.encode()).encode(),
        ),
    );
    eventually("our answer, proving the position was taken", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
            .filter(|msg| msg.cr == CommandResponse::Response)
            .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
            .find(|frame| frame.ctype == proto_bluetooth_audio::Ctype::Stable)
    })
    .await;

    // A CHANGED notification ends the subscription (AVRCP notifications are one-shot), so
    // position must be re-registered or it moves exactly twice and then stops.
    let events = registered_events(&transport);
    let positions: Vec<u32> = events
        .iter()
        .filter(|(id, _)| *id == proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED)
        .map(|(_, interval)| *interval)
        .collect();
    assert!(
        positions.len() > 1,
        "position must be re-subscribed after each CHANGED: {events:x?}"
    );
    // …and re-subscribed with a *reporting interval*, not with 0. The renewal is what a
    // phone spends almost all of its time registered under, so a Target that honours the
    // field literally reports position once and then never again.
    assert!(
        positions.iter().all(|interval| *interval > 0),
        "every position registration needs a nonzero interval, renewals included: {positions:?}"
    );
}

/// Drive a phone from connected to streaming, returning the pieces a test needs after.
///
/// Extracted so the *post*-START paths can be exercised at all: everything before this
/// point had tests and everything after it — suspend, restart, reconfigure, a second
/// phone — had none, which is how a stream that could never resume would have gone
/// unnoticed.
async fn stream_up(
    transport: &ScriptedTransport,
    signaling_cid: u16,
    media_cid: u16,
) -> (Cid, Cid, Seid) {
    let (signaling, _) = open_channel(transport, Psm::AVDTP, signaling_cid).await;
    let discover = avdtp(transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = eventually("an aptX endpoint", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .nth(2)
    })
    .await;

    let chosen = CodecCapability::AptX {
        rates: SampleRates::HZ_48000,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(transport, signaling, 3, Signal::SetConfiguration, &set).await;
    avdtp(transport, signaling, 4, Signal::Open, &[seid.shifted()]).await;

    let (media, _) = open_channel(transport, Psm::AVDTP, media_cid).await;
    avdtp(transport, signaling, 5, Signal::Start, &[seid.shifted()]).await;
    (signaling, media, seid)
}

/// One aptX media packet, distinguishable by its first byte.
fn media_packet(marker: u8) -> Bytes {
    let mut payload = BytesMut::new();
    payload.put_slice(&[marker, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]);
    payload.freeze()
}

#[tokio::test]
async fn a_stream_that_is_suspended_can_be_started_again() {
    // Pausing on the phone sends SUSPEND, pressing play sends START. `sink_flow.rs`
    // covers that at the state-machine level and nothing covered it end to end — so a
    // resume that silently produced no more audio, which is the shape of most of the bugs
    // in this file, would have gone unnoticed.
    let (transport, mut rx) = connected().await;
    let (signaling, media, seid) = stream_up(&transport, 0x0040, 0x0041).await;

    let msg = eventually("an audio session event", || rx.try_recv().ok()).await;
    let SessionEvent::Audio { source, .. } = msg.event else {
        panic!("expected an audio session, got {:?}", msg.event);
    };
    let FrameSource::Encoded(mut frames) = source else {
        panic!("audio must arrive as encoded frames");
    };

    push_pdu(&transport, &L2capPdu::new(media, media_packet(0x11)));
    let first = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("a frame before the pause")
        .expect("open");
    assert_eq!(first.data[0], 0x11);

    // Pause.
    let reply = avdtp(&transport, signaling, 6, Signal::Suspend, &[seid.shifted()]).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "a suspend from OPEN is legal"
    );

    // Play again. The configuration survives a suspend, so this needs no re-negotiation.
    let reply = avdtp(&transport, signaling, 7, Signal::Start, &[seid.shifted()]).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "a restart after suspend must be accepted, or the phone can never resume"
    );

    push_pdu(&transport, &L2capPdu::new(media, media_packet(0x22)));
    let second = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("audio must flow again after a restart")
        .expect("the session must not have been torn down by the pause");
    assert_eq!(
        second.data[0], 0x22,
        "and it must be the *new* packet, not a replay"
    );
}

#[tokio::test]
async fn a_reconfigured_stream_gets_a_session_at_the_new_rate() {
    // The adapter half of RECONFIGURE. `sink_flow.rs` proves the state machine validates
    // and accepts it; this proves the *consequence* — that the audio session opened at the
    // old rate is torn down and a new one opened at the new one. Without that the sink
    // agrees to 44.1 kHz, the phone re-encodes, and the decoder and output device are both
    // still sized for 48: the room gets the wrong pitch and nothing is logged.
    let (transport, mut rx) = connected().await;
    let (signaling, _media, seid) = stream_up(&transport, 0x0040, 0x0041).await;

    let msg = eventually("the first audio session", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::Audio { .. }))
    })
    .await;
    let SessionEvent::Audio { format, .. } = msg.event else {
        unreachable!("filtered")
    };
    assert_eq!(format.sample_rate(), 48_000);

    // Back to OPEN — RECONFIGURE is only legal there — then change the rate.
    avdtp(&transport, signaling, 6, Signal::Suspend, &[seid.shifted()]).await;
    let at_44k = CodecCapability::AptX {
        rates: SampleRates::HZ_44100,
        channels: ChannelModes::JOINT_STEREO,
    };
    let codec = at_44k.encode();
    let mut payload = vec![seid.shifted(), 0x07];
    payload.push(u8::try_from(codec.len()).unwrap());
    payload.extend_from_slice(&codec);
    let reply = avdtp(&transport, signaling, 7, Signal::Reconfigure, &payload).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept
    );

    // Play again: the session that opens must be the *new* shape.
    avdtp(&transport, signaling, 8, Signal::Start, &[seid.shifted()]).await;
    let msg = eventually("a session at the reconfigured rate", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::Audio { .. }))
    })
    .await;
    let SessionEvent::Audio { format, .. } = msg.event else {
        unreachable!("filtered")
    };
    assert_eq!(
        format.sample_rate(),
        44_100,
        "the decoder must follow the sender's new rate, not the one it opened with"
    );
}

// ---------------------------------------------------------------------------------------
// A stream that is running, and the things that happen to it (#95).
//
// Everything above gets a stream *up*. What follows is thin in exactly the place the panel
// is not: the codec that every sender falls back to, the packet that will not parse, the
// queue that fills, the volume knob on the phone, and the two ways a sender ends a stream
// without disconnecting. The shape of each of these, when it breaks, is the same and is
// the reason they are worth pinning: the panel looks connected and plays nothing, or plays
// it at the wrong pitch, and says nothing about either.
// ---------------------------------------------------------------------------------------

/// One RTP media packet carrying `payload`, which is what every codec here but classic
/// aptX rides in.
fn rtp(sequence: u16, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(12 + payload.len());
    buf.put_u8(0x80); // version 2, no padding, no extension, no CSRCs
    buf.put_u8(96); // dynamic payload type
    buf.put_u16(sequence);
    buf.put_u32(u32::from(sequence) * 128); // a timestamp that advances with the stream
    buf.put_u32(0xDEAD_BEEF); // ssrc
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// One SBC media packet: the one-byte frame-count header A2DP prepends, then a frame whose
/// header states the bitpool it was coded at.
fn sbc_packet(sequence: u16, bitpool: u8) -> Bytes {
    // Syncword, stream parameters, bitpool, CRC, and two bytes standing in for the coded
    // subband data — this side never decodes it, it only has to survive framing.
    let frame = [0x9C, 0x31, bitpool, 0x00, 0xAA, 0xBB];
    let mut payload = vec![0x01]; // one frame in this packet
    payload.extend_from_slice(&frame);
    rtp(sequence, &payload)
}

/// Bring a stream up on the endpoint advertising `chosen`'s codec, configured as `chosen`.
///
/// The endpoint is found by *asking* — discover, then get the last endpoint's capabilities
/// and check them — rather than by counting into the table the way the aptX helper above
/// does. SBC is always advertised and always last (a sink without it is not an A2DP sink),
/// so this pins that ordering as well as using it.
async fn stream_up_as(
    transport: &ScriptedTransport,
    signaling_cid: u16,
    media_cid: u16,
    chosen: &CodecCapability,
) -> (Cid, Cid, Seid) {
    let (signaling, _) = open_channel(transport, Psm::AVDTP, signaling_cid).await;
    let discover = avdtp(transport, signaling, 1, Signal::Discover, &[]).await;
    let seid = eventually("the last endpoint", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .next_back()
    })
    .await;

    let caps = avdtp(
        transport,
        signaling,
        2,
        Signal::GetAllCapabilities,
        &[seid.shifted()],
    )
    .await;
    let advertised = proto_bluetooth_audio::avdtp::find_codec_capability(&caps.payload).unwrap();
    assert_eq!(
        advertised.audio_codec(),
        chosen.audio_codec(),
        "the endpoint asked for is not the one the table put here"
    );

    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let reply = avdtp(transport, signaling, 3, Signal::SetConfiguration, &set).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "configuration should be accepted"
    );
    avdtp(transport, signaling, 4, Signal::Open, &[seid.shifted()]).await;
    let (media, _) = open_channel(transport, Psm::AVDTP, media_cid).await;
    avdtp(transport, signaling, 5, Signal::Start, &[seid.shifted()]).await;
    (signaling, media, seid)
}

/// The SBC capability a sender narrows ours down to: one rate, one channel mode.
fn sbc_at(rates: SampleRates) -> CodecCapability {
    CodecCapability::Sbc {
        rates,
        channels: ChannelModes::JOINT_STEREO,
        block_lengths: 0b1000, // 16 blocks
        subbands: 0b01,        // 8 subbands
        allocations: 0b10,     // loudness
        min_bitpool: 2,
        max_bitpool: 53,
    }
}

/// Wait for the audio session an established stream opens, and take its frames.
async fn audio_session(
    rx: &mut mpsc::Receiver<SourceMessage>,
) -> (
    tokio::sync::mpsc::Receiver<castaway_core::EncodedFrame>,
    castaway_core::AudioFormat,
) {
    let msg = eventually("an audio session event", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::Audio { .. }))
    })
    .await;
    let SessionEvent::Audio { source, format, .. } = msg.event else {
        unreachable!("filtered")
    };
    let FrameSource::Encoded(frames) = source else {
        panic!("audio must arrive as encoded frames");
    };
    (frames, format)
}

#[tokio::test]
async fn an_sbc_stream_reaches_the_pipeline_the_way_aptx_does() {
    // The only end-to-end codec here was aptX, chosen by counting to the third endpoint —
    // so the mandatory one, the one every sender falls back to when the radio is bad and
    // the only one whose decoder is ours rather than ffmpeg's, had never carried a byte
    // through the adapter. `a_restricted_codec_table_advertises_only_what_it_was_given`
    // restricts a build to SBC but only checks what gets *advertised*.
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;

    let (mut frames, format) = audio_session(&mut rx).await;
    assert_eq!(
        format.sample_rate(),
        44_100,
        "the negotiated rate must reach the decoder"
    );

    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(1, 35)));
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("an SBC packet should become a frame")
        .expect("the frame channel should be open");
    assert_eq!(frame.audio_codec, Some(AudioCodec::Sbc));
    // The RTP header and the one-byte frame count are framing, not audio. Leaving either
    // in shifts every frame and decodes to noise, silently — which is the whole reason
    // this assertion is on the bytes and not on the count.
    assert_eq!(
        &frame.data[..],
        &[0x9C, 0x31, 35, 0x00, 0xAA, 0xBB],
        "the frame must start at the SBC syncword"
    );
}

#[tokio::test]
async fn a_packet_that_cannot_be_depacketized_leaves_the_stream_running() {
    // `on_media`'s error arm is the one that produces this project's signature failure —
    // a connected phone, a running session, a populated now-playing card, and total
    // silence. Nothing had ever entered it, so nothing said whether a bad packet costs a
    // frame or costs the session.
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;

    // Too short to be an RTP packet at all — the shape a truncated ACL reassembly or a
    // sender with a different idea of the framing produces.
    push_pdu(
        &transport,
        &L2capPdu::new(media, Bytes::from_static(&[0x80, 0x60, 0x00])),
    );
    // …and one whose RTP header is fine but which carries nothing after the codec header.
    push_pdu(&transport, &L2capPdu::new(media, rtp(2, &[0x01])));

    // The next good packet still arrives, which is the whole claim: a packet we cannot
    // parse is a dropped frame, not a dead session.
    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(3, 35)));
    let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("the stream must survive a packet it cannot parse")
        .expect("the frame channel should still be open");
    assert_eq!(
        &frame.data[..],
        &[0x9C, 0x31, 35, 0x00, 0xAA, 0xBB],
        "and the frame that arrives must be the good one, not a mangled earlier packet"
    );
    assert!(
        !session_ended(&mut rx),
        "a malformed packet must not end the session"
    );
}

/// Whether the session has been told to end, without waiting for one that is not coming.
fn session_ended(rx: &mut mpsc::Receiver<SourceMessage>) -> bool {
    std::iter::from_fn(|| rx.try_recv().ok()).any(|m| matches!(m.event, SessionEvent::End))
}

#[tokio::test]
async fn a_queue_that_fills_drops_frames_rather_than_the_session() {
    // The `Full` and `Closed` arms of the same `try_send` are deliberately distinguished —
    // the difference between a hiccup and a dead session, and they used to collapse into
    // one `is_err()`. `Closed` is covered by the pipeline-lets-go test above; `Full` was
    // not covered at all, and it is the one that must *not* end anything.
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;

    // Past the queue's depth without reading a single one, which is what a decode thread
    // that has stalled looks like from here.
    for sequence in 0..(AUDIO_QUEUE_DEPTH + 64) {
        push_pdu(
            &transport,
            &L2capPdu::new(media, sbc_packet(u16::try_from(sequence).unwrap(), 35)),
        );
    }

    // Drain what did fit, then prove the stream is still live: a frame pushed after the
    // overflow still arrives. A dropped frame is a hiccup; this is what says it stayed one.
    // The queue is full long before the flood ends, so waiting for it to reach its depth is
    // waiting for something that has already happened by the time the adapter has caught up.
    eventually("the queue to fill", || {
        (frames.len() >= AUDIO_QUEUE_DEPTH).then_some(())
    })
    .await;
    let drained = std::iter::from_fn(|| frames.try_recv().ok()).count();
    assert_eq!(
        drained, AUDIO_QUEUE_DEPTH,
        "the queue should have held its whole depth"
    );
    assert!(
        !session_ended(&mut rx),
        "a full queue must not end the session"
    );

    // A packet coded at a bitpool nothing in the flood used, so it is identifiable, and
    // read for as long as the backlog takes to clear rather than expected next: the
    // adapter is still delivering the overflow while this is sent, and which frame arrives
    // first is a race. What is being claimed is that audio *keeps flowing*, not that a
    // queue that just overflowed is empty on the instant.
    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(9000, 41)));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let frame = tokio::time::timeout_at(deadline, frames.recv())
            .await
            .expect("audio must keep flowing after the queue has overflowed")
            .expect("the frame channel should still be open");
        if frame.data[2] == 41 {
            break;
        }
    }
}

/// The depth of the adapter's audio queue, mirrored from `adapter.rs`. Not public, and not
/// worth making public — what matters here is only that the test overshoots it.
const AUDIO_QUEUE_DEPTH: usize = 256;

/// Build an AVRCP vendor-dependent *command*, as a phone sends one to us.
fn avrcp_command(
    transaction: u8,
    ctype: proto_bluetooth_audio::Ctype,
    pdu: u8,
    params: &[u8],
) -> Bytes {
    proto_bluetooth_audio::AvctpMessage::command(
        transaction,
        proto_bluetooth_audio::avrcp::vendor_command(ctype, pdu, params).encode(),
    )
    .encode()
}

/// Every vendor-dependent AVRCP *response* the adapter has sent, as (pdu, ctype, params).
fn avrcp_responses(
    transport: &ScriptedTransport,
) -> Vec<(u8, proto_bluetooth_audio::Ctype, Bytes)> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter(|msg| msg.cr == CommandResponse::Response)
        .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
        .filter_map(|frame| {
            proto_bluetooth_audio::VendorPdu::parse(&frame.operands)
                .ok()
                .map(|vendor| (vendor.pdu_id, frame.ctype, vendor.parameters))
        })
        .collect()
}

#[tokio::test]
async fn the_volume_slider_on_the_phone_moves_the_panel_and_is_echoed_back() {
    // #69 made the phone authoritative over volume, and nothing in this crate had ever
    // sent us a `SET_ABSOLUTE_VOLUME`. Two halves, and both matter: the value has to reach
    // the session (or the slider moves nothing), and it has to be echoed (or the phone's
    // own volume UI springs back to where it was, which reads as the panel refusing).
    let (transport, mut rx) = connected().await;
    let (_signaling, _media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, _) = audio_session(&mut rx).await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    // 0x40 of 0x7F — a little under half, and not a value a defaulted path would produce.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                3,
                proto_bluetooth_audio::Ctype::Control,
                proto_bluetooth_audio::avrcp::pdu::SET_ABSOLUTE_VOLUME,
                &[0x40],
            ),
        ),
    );

    let volume = eventually("the volume reaching the session", || {
        rx.try_recv().ok().and_then(|m| match m.event {
            SessionEvent::Control(castaway_core::ControlTxn::Volume(v)) => Some(v),
            _ => None,
        })
    })
    .await;
    assert!(
        (volume.position() - 0x40 as f32 / 127.0).abs() < 0.01,
        "the panel must follow the phone's slider, got {volume:?}"
    );

    let echo = eventually("the accepted value echoed back", || {
        avrcp_responses(&transport)
            .into_iter()
            .find(|(pdu, ..)| *pdu == proto_bluetooth_audio::avrcp::pdu::SET_ABSOLUTE_VOLUME)
    })
    .await;
    assert_eq!(
        echo.1,
        proto_bluetooth_audio::Ctype::Accepted,
        "a volume we honoured must be answered ACCEPTED"
    );
    assert_eq!(
        echo.2.first().copied(),
        Some(0x40),
        "the echo must carry the value we accepted, or the phone's UI sticks"
    );
}

#[tokio::test]
async fn a_notification_the_phone_registers_on_us_is_answered_rather_than_left_hanging() {
    // We register notifications *on* phones all the time; a phone registering one on us
    // used to reach a handler that only modelled the response direction, fall through to
    // the catch-all, and get `NOT IMPLEMENTED` — for every event, including the one we
    // advertise (#211). The command arm exists now, and this pins its refusing half:
    // PLAYBACK_STATUS_CHANGED is not in SUPPORTED_EVENTS — we are not a Target with a
    // playlist — so the answer stays NOT IMPLEMENTED. What must never happen is silence:
    // AVCTP has no "ignored", so a stack that gets none waits out its transaction timeout
    // and some abort the link.
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                5,
                proto_bluetooth_audio::Ctype::Notify,
                proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION,
                &[
                    proto_bluetooth_audio::avrcp::event::PLAYBACK_STATUS_CHANGED,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
        ),
    );

    let answer = eventually("an answer to the registration", || {
        avrcp_responses(&transport)
            .into_iter()
            .find(|(pdu, ..)| *pdu == proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION)
    })
    .await;
    assert_eq!(
        answer.1,
        proto_bluetooth_audio::Ctype::NotImplemented,
        "we are not a Target with events to report, and saying nothing is not an option"
    );
}

/// Every `REGISTER_NOTIFICATION` response the adapter has sent, keeping the AVCTP label —
/// which is the substance here: the CHANGED half of a notification answers the
/// *registration's* transaction, not whatever command moved the value.
fn notification_responses(
    transport: &ScriptedTransport,
) -> Vec<(u8, proto_bluetooth_audio::Ctype, Bytes)> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter(|msg| msg.cr == CommandResponse::Response)
        .filter_map(|msg| {
            proto_bluetooth_audio::AvcFrame::decode(&msg.body)
                .ok()
                .map(|frame| (msg.transaction, frame))
        })
        .filter_map(|(transaction, frame)| {
            proto_bluetooth_audio::VendorPdu::parse(&frame.operands)
                .ok()
                .filter(|vendor| {
                    vendor.pdu_id == proto_bluetooth_audio::avrcp::pdu::REGISTER_NOTIFICATION
                })
                .map(|vendor| (transaction, frame.ctype, vendor.parameters))
        })
        .collect()
}

#[tokio::test]
async fn registering_the_advertised_volume_event_gets_interim_now_and_changed_on_the_move() {
    // The mirror image of the refusal above, and the half #211 found missing:
    // GET_CAPABILITIES advertises exactly one event, VOLUME_CHANGED, and the registration
    // a phone sends on the strength of that answer was falling through to the same
    // NOT IMPLEMENTED. A controller is entitled to read that as "absolute volume
    // unsupported" and never offer it — the volume-rocker-does-nothing failure. The
    // contract has three legs: INTERIM with the current value at registration, CHANGED
    // under the *registration's* label when the level moves, and nothing further until
    // the peer re-arms — AVRCP notifications are one-shot.
    use proto_bluetooth_audio::avrcp::{event, pdu};
    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                5,
                proto_bluetooth_audio::Ctype::Notify,
                pdu::REGISTER_NOTIFICATION,
                &[event::VOLUME_CHANGED, 0, 0, 0, 0],
            ),
        ),
    );
    let interim = eventually("the INTERIM answering the registration", || {
        notification_responses(&transport)
            .into_iter()
            .find(|(_, ctype, _)| *ctype == proto_bluetooth_audio::Ctype::Interim)
    })
    .await;
    assert_eq!(
        interim.0, 5,
        "the INTERIM must ride the registration's label"
    );
    assert_eq!(
        interim.2.as_ref(),
        &[event::VOLUME_CHANGED, 0x7F],
        "the current level: nothing has moved it, and the pipeline's gain starts at full"
    );

    // The phone turns its slider down. The acceptance is also the event the registration
    // asked to hear about, so both go out — and the CHANGED answers transaction 5, not 6.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                6,
                proto_bluetooth_audio::Ctype::Control,
                pdu::SET_ABSOLUTE_VOLUME,
                &[0x40],
            ),
        ),
    );
    let changed = eventually("the CHANGED completing the notification", || {
        notification_responses(&transport)
            .into_iter()
            .find(|(_, ctype, _)| *ctype == proto_bluetooth_audio::Ctype::Changed)
    })
    .await;
    assert_eq!(
        changed.0, 5,
        "CHANGED answers the registration, not the command that moved the volume"
    );
    assert_eq!(changed.2.as_ref(), &[event::VOLUME_CHANGED, 0x40]);

    // A second move with no registration outstanding, then a re-registration. Processing
    // is in arrival order, so once the second INTERIM is visible, any CHANGED the second
    // move wrongly produced would be visible too.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                7,
                proto_bluetooth_audio::Ctype::Control,
                pdu::SET_ABSOLUTE_VOLUME,
                &[0x20],
            ),
        ),
    );
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                8,
                proto_bluetooth_audio::Ctype::Notify,
                pdu::REGISTER_NOTIFICATION,
                &[event::VOLUME_CHANGED, 0, 0, 0, 0],
            ),
        ),
    );
    let rearmed = eventually("the INTERIM answering the re-registration", || {
        notification_responses(&transport)
            .into_iter()
            .find(|(transaction, ctype, _)| {
                *transaction == 8 && *ctype == proto_bluetooth_audio::Ctype::Interim
            })
    })
    .await;
    assert_eq!(
        rearmed.2.as_ref(),
        &[event::VOLUME_CHANGED, 0x20],
        "the INTERIM reports the tracked level, not a constant"
    );
    let fired = notification_responses(&transport)
        .into_iter()
        .filter(|(_, ctype, _)| *ctype == proto_bluetooth_audio::Ctype::Changed)
        .count();
    assert_eq!(
        fired, 1,
        "a notification is one-shot: the move at 0x20 had no registration to answer"
    );
}

#[tokio::test]
async fn an_abort_ends_the_session_and_leaves_the_endpoint_free() {
    // ABORT is the sender's escape hatch: legal from any state, used when it has given up
    // on the exchange rather than finished with it. `sink_flow.rs` accepts one at the
    // state-machine level; what was never checked is the consequence — whether the session
    // ends, and whether the endpoint it held is available to the next sender or is stuck
    // `in_use` for the life of the process.
    let (transport, mut rx) = connected().await;
    let (signaling, _media, seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, _) = audio_session(&mut rx).await;

    let reply = avdtp(&transport, signaling, 6, Signal::Abort, &[seid.shifted()]).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "an abort is legal from any state"
    );
    eventually("the session ending", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::End))
    })
    .await;

    // The next sender has to be able to take the endpoint, and there are two separate
    // ways it could find it held. What it *sees* first is DISCOVER's in-use bit — the only
    // place that flag is ever published — so an endpoint left flagged is one a sender
    // skips before it tries anything.
    assert!(
        !endpoint_in_use(&transport, signaling, 7, seid).await,
        "the aborted endpoint is still advertised as in use: a sender will not even try it"
    );
    // …and then whether the configuration is actually accepted, which is the sink's state
    // rather than the flag, and can be stuck independently of it.
    let chosen = sbc_at(SampleRates::HZ_48000);
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let reply = avdtp(&transport, signaling, 8, Signal::SetConfiguration, &set).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "the aborted endpoint is still held: the next cast gets nothing"
    );
}

/// Whether a fresh DISCOVER reports `seid` as in use.
///
/// The flag rides bit 1 of the first byte of each endpoint's pair, and it is the only
/// place AVDTP publishes it — a sender reads it here and skips a busy endpoint without
/// ever sending a SET_CONFIGURATION, so an endpoint left flagged is invisible to the
/// state machine that would otherwise refuse it.
async fn endpoint_in_use(
    transport: &ScriptedTransport,
    signaling: Cid,
    transaction: u8,
    seid: Seid,
) -> bool {
    let discover = avdtp(transport, signaling, transaction, Signal::Discover, &[]).await;
    discover
        .payload
        .chunks(2)
        .find(|c| Seid::from_shifted(c[0]).ok() == Some(seid))
        .is_some_and(|c| c[0] & 0x02 != 0)
}

#[tokio::test]
async fn a_closed_stream_can_be_configured_again_at_a_new_rate() {
    // CLOSE is how a well-behaved sender ends a stream it intends to reopen — switching
    // codec, or changing rate on a track boundary, without dropping the link. RECONFIGURE
    // (covered above) keeps the configuration; this throws it away and builds a new one,
    // and it is the path that leaves an endpoint stuck if the teardown is incomplete.
    let (transport, mut rx) = connected().await;
    let (signaling, _media, seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, format) = audio_session(&mut rx).await;
    assert_eq!(format.sample_rate(), 44_100);

    let reply = avdtp(&transport, signaling, 6, Signal::Close, &[seid.shifted()]).await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept
    );
    eventually("the session ending", || {
        rx.try_recv()
            .ok()
            .filter(|m| matches!(m.event, SessionEvent::End))
    })
    .await;

    // Straight back up at a different rate, which is what a phone changing tracks between
    // a 44.1 kHz and a 48 kHz album does.
    assert!(
        !endpoint_in_use(&transport, signaling, 7, seid).await,
        "a closed endpoint must be advertised as free again"
    );

    let chosen = sbc_at(SampleRates::HZ_48000);
    let codec = chosen.encode();
    let mut set = vec![seid.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp(&transport, signaling, 8, Signal::SetConfiguration, &set).await;
    avdtp(&transport, signaling, 9, Signal::Open, &[seid.shifted()]).await;
    let (_media, _) = open_channel(&transport, Psm::AVDTP, 0x0042).await;
    avdtp(&transport, signaling, 10, Signal::Start, &[seid.shifted()]).await;

    let (_frames, format) = audio_session(&mut rx).await;
    assert_eq!(
        format.sample_rate(),
        48_000,
        "the second session must be sized for what was negotiated the second time"
    );
}

/// Drive an adapter built from `config` to the point where `peer`'s ACL link exists.
///
/// The two-adapter half of [`connected`]: a test that restarts the receiver, or connects a
/// second phone, needs to choose the configuration and the address rather than take the
/// one address the helper above hardcodes.
async fn connected_as(
    transport: &Arc<ScriptedTransport>,
    config: BluetoothConfig,
    peer: &str,
) -> mpsc::Receiver<SourceMessage> {
    let adapter = Arc::new(BluetoothAdapter::new(
        Arc::clone(transport) as Arc<dyn HciTransport>,
        config,
    ));
    let (tx, rx) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Bluetooth, "listener"), tx);
    tokio::spawn(Arc::clone(&adapter).run(sink));
    eventually("scan enable", || {
        transport
            .sent_commands()
            .contains(&substrate_hci::OpCode::WRITE_SCAN_ENABLE)
            .then_some(())
    })
    .await;
    page_us(transport, peer).await;
    rx
}

/// A phone paging the receiver, and the receiver accepting.
async fn page_us(transport: &ScriptedTransport, peer: &str) {
    let addr: BdAddr = peer.parse().unwrap();
    let before = transport
        .sent_commands()
        .iter()
        .filter(|c| **c == substrate_hci::OpCode::ACCEPT_CONNECTION_REQUEST)
        .count();
    let mut params = addr.to_wire().to_vec();
    params.extend_from_slice(&[0x0C, 0x02, 0x5A]); // class of device
    params.push(0x01); // ACL
    transport.push(HciPacket::Event {
        code: code::CONNECTION_REQUEST,
        params: Bytes::from(params),
    });
    eventually("connection accepted", || {
        (transport
            .sent_commands()
            .iter()
            .filter(|c| **c == substrate_hci::OpCode::ACCEPT_CONNECTION_REQUEST)
            .count()
            > before)
            .then_some(())
    })
    .await;
}

/// The parameters of the last command of `opcode` the adapter sent.
fn last_command(transport: &ScriptedTransport, want: substrate_hci::OpCode) -> Option<Bytes> {
    transport
        .sent()
        .into_iter()
        .filter_map(|p| match p {
            HciPacket::Command { opcode, params } if opcode == want => Some(params),
            _ => None,
        })
        .next_back()
}

#[tokio::test]
async fn a_bonded_phone_reconnects_after_a_restart_and_gets_straight_to_audio() {
    // #68's whole promise: a guest pairs once and every visit afterwards is silent — no
    // prompt, no confirmation, no re-pairing. `host.rs` covers the key store as a pure
    // unit; nothing had ever carried a key across a *restart* and out the other side to a
    // second stream, which is the only form in which the promise is actually made.
    let first = transport();
    let bonded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = BluetoothConfig {
        decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
        on_paired: Some(Arc::new({
            let bonded = Arc::clone(&bonded);
            move |addr, key| {
                if let Some(key) = key {
                    bonded.lock().unwrap().push((addr, key));
                }
            }
        })),
        ..BluetoothConfig::default()
    };
    let _rx = connected_as(&first, config, PEER).await;

    // The controller finishes pairing and hands up the key, which is the moment the
    // receiver is supposed to write it down.
    let addr: BdAddr = PEER.parse().unwrap();
    let key = [0x5Au8; 16];
    let mut params = addr.to_wire().to_vec();
    params.extend_from_slice(&key);
    params.push(0x04); // unauthenticated combination key, P-192
    first.push(HciPacket::Event {
        code: code::LINK_KEY_NOTIFICATION,
        params: Bytes::from(params),
    });
    let stored = eventually("the key being persisted", || {
        bonded.lock().unwrap().first().copied()
    })
    .await;
    assert_eq!(
        stored.0, addr,
        "the key must be filed under the right phone"
    );

    // Restart: a second adapter, a second controller, nothing in common with the first
    // but what was written down.
    let restarted = transport();
    let mut rx = connected_as(
        &restarted,
        BluetoothConfig {
            decodable: proto_bluetooth_audio::codec::ALL.to_vec(),
            link_keys: vec![stored],
            ..BluetoothConfig::default()
        },
        PEER,
    )
    .await;

    // The controller asks who this is. A receiver that has forgotten answers negatively,
    // and the phone prompts its owner to pair again — in a hackerspace, at the panel,
    // with the music already playing on someone else's phone.
    restarted.push(HciPacket::Event {
        code: code::LINK_KEY_REQUEST,
        params: Bytes::from(addr.to_wire().to_vec()),
    });
    let reply = eventually("the stored key being offered", || {
        last_command(&restarted, substrate_hci::OpCode::LINK_KEY_REQUEST_REPLY)
    })
    .await;
    assert!(
        !restarted
            .sent_commands()
            .contains(&substrate_hci::OpCode::LINK_KEY_REQUEST_NEGATIVE_REPLY),
        "a bonded phone must never be asked to pair again"
    );
    assert_eq!(&reply[..6], &addr.to_wire(), "answered for the right phone");
    assert_eq!(&reply[6..22], &key, "and with the key that was stored");

    // …and the point of all of it: audio, without a human touching anything.
    let (_signaling, media, _seid) =
        stream_up_as(&restarted, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;
    push_pdu(&restarted, &L2capPdu::new(media, sbc_packet(1, 35)));
    tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("a reconnected phone must reach the speakers")
        .expect("the frame channel should be open");
}

#[tokio::test]
async fn a_second_phone_that_starts_playing_takes_the_speakers_and_pauses_the_first() {
    // Two phones at once, which is a room with two people in it. The policy is #68's —
    // one phone at a time owns the speakers, and the one that loses is *told*, with an
    // AVRCP pause, rather than left playing into a decoder that has stopped listening.
    //
    // Pinned rather than assumed because #72 proposes changing it: blending several
    // senders instead of preempting. When that lands this test is what says what the old
    // behaviour was, and it should be rewritten rather than deleted.
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;
    // The first phone needs a control channel, or there is nowhere to send the pause and
    // the preemption is silently skipped.
    let (_, first_avctp_peer) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(1, 35)));
    tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("the first phone should be playing")
        .expect("open");

    // A second phone connects on a link of its own and starts.
    page_us(&transport, PEER2).await;
    let (signaling2, _) = open_channel_on(&transport, HANDLE2, Psm::AVDTP, 0x0060).await;
    let discover = avdtp_on(&transport, HANDLE2, signaling2, 1, Signal::Discover, &[]).await;
    let seid2 = eventually("the last endpoint on the second link", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .next_back()
    })
    .await;
    let chosen = sbc_at(SampleRates::HZ_48000);
    let codec = chosen.encode();
    let mut set = vec![seid2.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    let reply = avdtp_on(
        &transport,
        HANDLE2,
        signaling2,
        3,
        Signal::SetConfiguration,
        &set,
    )
    .await;
    assert_eq!(
        reply.message_type,
        proto_bluetooth_audio::avdtp::MessageType::ResponseAccept,
        "the second phone gets its own endpoint, on its own link"
    );
    avdtp_on(
        &transport,
        HANDLE2,
        signaling2,
        4,
        Signal::Open,
        &[seid2.shifted()],
    )
    .await;
    let (_media2, _) = open_channel_on(&transport, HANDLE2, Psm::AVDTP, 0x0061).await;

    let before = sent_pdus(&transport).len();
    avdtp_on(
        &transport,
        HANDLE2,
        signaling2,
        5,
        Signal::Start,
        &[seid2.shifted()],
    )
    .await;

    // The second phone's session opens…
    let (_frames2, format2) = audio_session(&mut rx).await;
    assert_eq!(
        format2.sample_rate(),
        48_000,
        "the second session must be the second phone's negotiation, not the first's"
    );

    // …and the first is told to stop, on its own AVRCP channel. Without this it keeps
    // sending into a decoder that has moved on, and pause/play cannot recover it, because
    // it was never in a state that needed re-starting.
    let paused = eventually("the first phone being paused", || {
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            // 0x7C is PASS THROUGH, 0x46 the PAUSE operand.
            .find(|pdu| {
                pdu.cid == first_avctp_peer && pdu.payload.windows(2).any(|w| w == [0x7C, 0x46])
            })
    })
    .await;
    assert_eq!(
        paused.cid, first_avctp_peer,
        "the pause belongs to the phone that lost the speakers, not the one that took them"
    );
}

/// Bring a second phone up to `START`, taking the speakers from whoever has them.
///
/// Returns the index into `sent_pdus` from just before the START, so a caller can look at
/// only what the preemption itself produced.
async fn second_phone_takes_over(transport: &ScriptedTransport) -> usize {
    page_us(transport, PEER2).await;
    let (signaling2, _) = open_channel_on(transport, HANDLE2, Psm::AVDTP, 0x0060).await;
    let discover = avdtp_on(transport, HANDLE2, signaling2, 1, Signal::Discover, &[]).await;
    let seid2 = eventually("the last endpoint on the second link", || {
        discover
            .payload
            .chunks(2)
            .filter_map(|c| Seid::from_shifted(c[0]).ok())
            .next_back()
    })
    .await;
    let codec = sbc_at(SampleRates::HZ_48000).encode();
    let mut set = vec![seid2.shifted(), 0x04, 0x01, 0x00, 0x07];
    set.push(u8::try_from(codec.len()).unwrap());
    set.extend_from_slice(&codec);
    avdtp_on(
        transport,
        HANDLE2,
        signaling2,
        3,
        Signal::SetConfiguration,
        &set,
    )
    .await;
    avdtp_on(
        transport,
        HANDLE2,
        signaling2,
        4,
        Signal::Open,
        &[seid2.shifted()],
    )
    .await;
    let (_media2, _) = open_channel_on(transport, HANDLE2, Psm::AVDTP, 0x0061).await;

    let before = sent_pdus(transport).len();
    avdtp_on(
        transport,
        HANDLE2,
        signaling2,
        5,
        Signal::Start,
        &[seid2.shifted()],
    )
    .await;
    before
}

/// Wait for an AVDTP `SUSPEND` *command* addressed to `cid`, and return it.
async fn awaited_suspend(
    transport: &ScriptedTransport,
    what: &str,
    cid: Cid,
    from: usize,
) -> proto_bluetooth_audio::avdtp::Message {
    eventually(what, || {
        sent_pdus(transport)
            .into_iter()
            .skip(from)
            .filter(|pdu| pdu.cid == cid)
            .filter_map(|pdu| proto_bluetooth_audio::avdtp::Message::decode(&pdu.payload).ok())
            .find(|msg| {
                msg.signal == Signal::Suspend
                    && msg.message_type == proto_bluetooth_audio::avdtp::MessageType::Command
            })
    })
    .await
}

#[tokio::test]
async fn a_preempted_phone_is_told_to_stop_sending_and_not_only_to_pause() {
    // The AVRCP pause is what the person holding the phone sees; it is not what stops the
    // radio. A phone that ignores the keypress went on transmitting into a session that
    // had already been torn down — still holding its share of the piconet, still spending
    // ACL credits against the phone that actually won — and nothing at default log level
    // said so. The last word about it was "pausing a preempted phone" (#92).
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;
    let (_, first_avctp_peer) = open_channel(&transport, Psm::AVCTP, 0x0050).await;
    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(1, 35)));
    tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("the first phone should be playing")
        .expect("open");

    let before = second_phone_takes_over(&transport).await;
    let (_frames2, _) = audio_session(&mut rx).await;

    // Both halves, and they are not alternatives.
    let suspend = awaited_suspend(
        &transport,
        "the first phone's stream being suspended",
        Cid::new(0x0040),
        before,
    )
    .await;
    assert_eq!(
        suspend.payload.first().copied(),
        // `stream_up_as` configures with an INT SEID of 1, so that is the endpoint on the
        // phone's side of this stream — the one a SUSPEND has to name. Ours would be an
        // endpoint the phone has never heard of.
        Some(1 << 2),
        "a SUSPEND names the peer's endpoint, from SET_CONFIGURATION's INT SEID"
    );
    assert!(
        sent_pdus(&transport)
            .into_iter()
            .skip(before)
            .any(|pdu| pdu.cid == first_avctp_peer
                && pdu.payload.windows(2).any(|w| w == [0x7C, 0x46])),
        "the AVRCP pause is what the phone's own screen reflects, and it still goes"
    );
}

#[tokio::test]
async fn a_preempted_phone_with_no_control_channel_is_still_told_to_stop() {
    // The case with no mitigation whatsoever: `pause_peer` returned before sending
    // anything when there was no AVCTP channel, so this phone was "preempted" by being
    // sent nothing at all. AVDTP is not optional the way AVRCP is — a streaming phone has
    // a signaling channel by construction, because that is how the stream was negotiated.
    let (transport, mut rx) = connected().await;
    let (_signaling, media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (mut frames, _) = audio_session(&mut rx).await;
    push_pdu(&transport, &L2capPdu::new(media, sbc_packet(1, 35)));
    tokio::time::timeout(Duration::from_secs(2), frames.recv())
        .await
        .expect("the first phone should be playing")
        .expect("open");

    let before = second_phone_takes_over(&transport).await;
    let (_frames2, _) = audio_session(&mut rx).await;

    let suspend = awaited_suspend(
        &transport,
        "a phone with no AVRCP channel still being told to stop",
        Cid::new(0x0040),
        before,
    )
    .await;
    assert_eq!(suspend.payload.first().copied(), Some(1 << 2));
}

/// Every vendor-dependent AVRCP *command* the adapter has sent, as (pdu, ctype, params).
///
/// The mirror of `avrcp_responses`, and the one that matters for anything we *ask* a
/// phone rather than answer it.
fn avrcp_commands(transport: &ScriptedTransport) -> Vec<(u8, proto_bluetooth_audio::Ctype, Bytes)> {
    sent_pdus(transport)
        .into_iter()
        .filter_map(|pdu| proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok())
        .filter(|msg| msg.cr == CommandResponse::Command)
        .filter_map(|msg| proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok())
        .filter_map(|frame| {
            proto_bluetooth_audio::VendorPdu::parse(&frame.operands)
                .ok()
                .map(|vendor| (vendor.pdu_id, frame.ctype, vendor.parameters))
        })
        .collect()
}

/// Wait for a vendor command with `pdu`, returning its parameters.
async fn awaited_command(transport: &ScriptedTransport, what: &str, pdu: u8) -> Bytes {
    eventually(what, || {
        avrcp_commands(transport)
            .into_iter()
            .find(|(id, _, _)| *id == pdu)
            .map(|(_, _, params)| params)
    })
    .await
}

/// A vendor-dependent AVRCP response, as a phone answers one of our commands.
fn phone_answer(pdu: u8, params: &[u8]) -> Bytes {
    proto_bluetooth_audio::AvctpMessage::command(
        0,
        proto_bluetooth_audio::avrcp::vendor_command(
            proto_bluetooth_audio::Ctype::Stable,
            pdu,
            params,
        )
        .encode(),
    )
    .encode()
}

#[tokio::test]
async fn shuffle_and_repeat_are_enumerated_before_they_are_offered() {
    // #76: the protocol has player application settings, the core model has shuffle and
    // repeat, and the adapter had neither — so a Bluetooth link advertised bare TRANSPORT
    // however capable the phone was.
    //
    // The whole interrogation, in the order it has to happen: which settings exist, which
    // values each takes, what they are now, and a subscription so the phone's own UI
    // reaches the strip. The value listings are *serial* on purpose — a 0x12 response does
    // not echo the attribute it is about, so two in flight is two lists we cannot tell
    // apart.
    use proto_bluetooth_audio::avrcp::{self, pdu};

    let (transport, mut rx) = connected().await;
    let (_signaling, _media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, _) = audio_session(&mut rx).await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    // 1. It asks, unprompted, as soon as AVRCP is up.
    let request = awaited_command(
        &transport,
        "the settings listing",
        pdu::LIST_SETTING_ATTRIBUTES,
    )
    .await;
    assert!(
        request.is_empty(),
        "ListPlayerApplicationSettingAttributes takes no parameters; BlueZ rejects one that does"
    );

    // 2. The phone lists repeat, shuffle, and something we do not implement.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_ATTRIBUTES, &[3, 0x02, 0x03, 0x7F]),
        ),
    );

    // 3. Values for repeat first, and *only* repeat — one at a time.
    let asked = awaited_command(
        &transport,
        "the repeat value listing",
        pdu::LIST_SETTING_VALUES,
    )
    .await;
    assert_eq!(&asked[..], &[0x02], "repeat, and one attribute per request");
    assert_eq!(
        avrcp_commands(&transport)
            .iter()
            .filter(|(id, _, _)| *id == pdu::LIST_SETTING_VALUES)
            .count(),
        1,
        "a second listing in flight would be a list we cannot attribute"
    );

    // This player does group-repeat and not all-track repeat, which is the case the
    // preference list exists for.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_VALUES, &[2, 0x01, 0x04]),
        ),
    );

    // 4. …then shuffle.
    let asked = eventually("the shuffle value listing", || {
        avrcp_commands(&transport)
            .into_iter()
            .filter(|(id, _, _)| *id == pdu::LIST_SETTING_VALUES)
            .nth(1)
            .map(|(_, _, params)| params)
    })
    .await;
    assert_eq!(&asked[..], &[0x03]);
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_VALUES, &[2, 0x01, 0x02]),
        ),
    );

    // 5. Only now does it read the current values, and subscribe to changes.
    let current = awaited_command(
        &transport,
        "the current settings read",
        pdu::GET_CURRENT_SETTINGS,
    )
    .await;
    assert_eq!(
        &current[..],
        &[2, 0x02, 0x03],
        "both listed attributes, count first"
    );
    eventually("a subscription to setting changes", || {
        avrcp_commands(&transport)
            .into_iter()
            .find(|(id, _, params)| {
                *id == pdu::REGISTER_NOTIFICATION
                    && params.first() == Some(&avrcp::event::SETTING_CHANGED)
            })
            .map(|_| ())
    })
    .await;

    // 6. The answer reaches the card. Group repeat folds into Context — naming the group
    //    needs the browsing channel, and a third repeat icon nobody can explain is worse.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::GET_CURRENT_SETTINGS, &[2, 0x03, 0x02, 0x02, 0x04]),
        ),
    );
    let now = eventually("shuffle and repeat on the card", || {
        rx.try_recv().ok().and_then(|m| match m.event {
            SessionEvent::NowPlaying(n) if n.shuffle.is_some() => Some(n),
            _ => None,
        })
    })
    .await;
    assert_eq!(now.shuffle, Some(true));
    assert_eq!(now.repeat, Some(castaway_core::RepeatMode::Context));
}

#[tokio::test]
async fn a_shuffle_press_is_a_setting_write_not_a_keypress() {
    // There is no passthrough key that means "shuffle", so this is a fork in the control
    // path rather than a fallback — and the value written has to be one the peer said it
    // accepts, which for this player is group-shuffle rather than the usual all-tracks.
    use castaway_core::ControlTxn;
    use proto_bluetooth_audio::avrcp::pdu;

    let (transport, mut rx) = connected().await;
    let (_signaling, _media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, _) = audio_session(&mut rx).await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    let control = eventually("the control surface", || {
        rx.try_recv().ok().and_then(|m| match m.event {
            SessionEvent::ControlSurface(c) => Some(c),
            _ => None,
        })
    })
    .await;
    // Before the listing lands the panel must not offer it: a lit button that answers
    // REJECTED is worse than no button.
    assert!(
        !control.capabilities().supports(&ControlTxn::Shuffle(true)),
        "nothing has said this player has shuffle yet"
    );

    awaited_command(
        &transport,
        "the settings listing",
        pdu::LIST_SETTING_ATTRIBUTES,
    )
    .await;
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_ATTRIBUTES, &[1, 0x03]),
        ),
    );
    awaited_command(
        &transport,
        "the shuffle value listing",
        pdu::LIST_SETTING_VALUES,
    )
    .await;
    // Group shuffle only — no all-tracks value on offer.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_VALUES, &[2, 0x01, 0x03]),
        ),
    );

    eventually("shuffle becoming available", || {
        control
            .capabilities()
            .supports(&ControlTxn::Shuffle(true))
            .then_some(())
    })
    .await;
    // …and repeat did not come with it. The listing is the gate, per setting.
    assert!(!control
        .capabilities()
        .supports(&ControlTxn::Repeat(castaway_core::RepeatMode::Context)));

    control.issue(ControlTxn::Shuffle(true)).await.unwrap();
    let write = awaited_command(&transport, "the setting write", pdu::SET_SETTING_VALUE).await;
    assert_eq!(
        &write[..],
        &[1, 0x03, 0x03],
        "one pair: shuffle, group — the value this player actually listed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_refuses_position_notifications_gets_polled_instead() {
    // #162, measured on an iPhone: it answers `NOT IMPLEMENTED` to
    // `PLAYBACK_POS_CHANGED`. Nothing else moves a scrubber — `PLAYBACK_STATUS_CHANGED`
    // reports play and pause, `TRACK_CHANGED` reports boundaries, and neither says where
    // in the track we are — so on the commonest sender we have, the position froze at
    // whatever it was when the track started and stayed there.
    //
    // The refusal names no event (an iPhone sends it with zero parameters), so the label
    // we registered under is the only thing that says which subscription was turned down.
    use proto_bluetooth_audio::avrcp::pdu;

    let (transport, _rx) = connected().await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    // Find the label the position subscription went out under.
    let label = eventually("a position-change registration", || {
        sent_pdus(&transport).into_iter().find_map(|pdu| {
            let msg = proto_bluetooth_audio::AvctpMessage::decode(&pdu.payload).ok()?;
            let frame = proto_bluetooth_audio::AvcFrame::decode(&msg.body).ok()?;
            let vendor = proto_bluetooth_audio::VendorPdu::parse(&frame.operands).ok()?;
            (vendor.pdu_id == pdu::REGISTER_NOTIFICATION
                && vendor.parameters.first()
                    == Some(&proto_bluetooth_audio::avrcp::event::PLAYBACK_POS_CHANGED))
            .then_some(msg.transaction)
        })
    })
    .await;

    let before = avrcp_commands(&transport)
        .iter()
        .filter(|(id, _, _)| *id == pdu::GET_PLAY_STATUS)
        .count();

    // The phone refuses it, exactly as an iPhone does: zero parameters, NOT IMPLEMENTED,
    // under the label we asked with.
    let refusal = proto_bluetooth_audio::AvctpMessage::command(
        label,
        proto_bluetooth_audio::avrcp::vendor_command(
            proto_bluetooth_audio::Ctype::NotImplemented,
            pdu::REGISTER_NOTIFICATION,
            &[],
        )
        .encode(),
    )
    .encode();
    push_pdu(&transport, &L2capPdu::new(avctp, refusal));

    // …and it is told the track is playing, so there is something to follow.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            avrcp_command(
                9,
                proto_bluetooth_audio::Ctype::Changed,
                pdu::REGISTER_NOTIFICATION,
                &[
                    proto_bluetooth_audio::avrcp::event::PLAYBACK_STATUS_CHANGED,
                    0x01, // PLAYING
                ],
            ),
        ),
    );

    // The first poll goes out immediately rather than a second into a track already
    // playing, and then keeps coming.
    let polls_after = |t: &ScriptedTransport| {
        avrcp_commands(t)
            .iter()
            .filter(|(id, _, _)| *id == pdu::GET_PLAY_STATUS)
            .count()
    };
    eventually("an immediate play-status poll", || {
        (polls_after(&transport) > before).then_some(())
    })
    .await;

    let one = polls_after(&transport);
    tokio::time::advance(Duration::from_secs(3)).await;
    // Nudge the loop so the paused clock is actually observed.
    push_pdu(
        &transport,
        &L2capPdu::new(avctp, Bytes::from_static(&[0u8])),
    );
    eventually("the poll repeating while playing", || {
        (polls_after(&transport) > one).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_metadata_read_does_not_wipe_shuffle_repeat_or_artwork() {
    // Reported from the panel: #76 landed, the phone exposed shuffle and repeat, and no
    // buttons appeared. The settings were being learned and then thrown away.
    //
    // `GetElementAttributes` describes the track. Everything else on the snapshot belongs
    // to the session and arrives from somewhere else — and the handler replaced the whole
    // snapshot, handing back only `state`. Phones re-read metadata constantly, so shuffle
    // and repeat returned to `None` within moments of being learned, and the transport
    // strip will not draw a button for a setting whose state it does not know.
    use proto_bluetooth_audio::avrcp::pdu;

    let (transport, mut rx) = connected().await;
    let (_signaling, _media, _seid) =
        stream_up_as(&transport, 0x0040, 0x0041, &sbc_at(SampleRates::HZ_44100)).await;
    let (_frames, _) = audio_session(&mut rx).await;
    let (avctp, _) = open_channel(&transport, Psm::AVCTP, 0x0050).await;

    // The player exposes both settings, and reports them on.
    awaited_command(
        &transport,
        "the settings listing",
        pdu::LIST_SETTING_ATTRIBUTES,
    )
    .await;
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            phone_answer(pdu::LIST_SETTING_ATTRIBUTES, &[2, 0x02, 0x03]),
        ),
    );
    for _ in 0..2 {
        awaited_command(&transport, "a value listing", pdu::LIST_SETTING_VALUES).await;
        push_pdu(
            &transport,
            &L2capPdu::new(
                avctp,
                phone_answer(pdu::LIST_SETTING_VALUES, &[3, 0x01, 0x02, 0x03]),
            ),
        );
    }
    awaited_command(
        &transport,
        "the current settings",
        pdu::GET_CURRENT_SETTINGS,
    )
    .await;
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            // shuffle all-tracks, repeat single-track
            phone_answer(pdu::GET_CURRENT_SETTINGS, &[2, 0x03, 0x02, 0x02, 0x02]),
        ),
    );

    let with_settings = eventually("shuffle and repeat on the card", || {
        rx.try_recv().ok().and_then(|m| match m.event {
            SessionEvent::NowPlaying(n) if n.shuffle.is_some() => Some(n),
            _ => None,
        })
    })
    .await;
    assert_eq!(with_settings.shuffle, Some(true));
    assert_eq!(with_settings.repeat, Some(castaway_core::RepeatMode::Track));

    // Now a perfectly ordinary metadata read — the thing a phone does constantly.
    push_pdu(
        &transport,
        &L2capPdu::new(
            avctp,
            attributes_response(&[
                (proto_bluetooth_audio::avrcp::attribute::TITLE, b"Derezzed"),
                (
                    proto_bluetooth_audio::avrcp::attribute::ARTIST,
                    b"Daft Punk",
                ),
            ]),
        ),
    );

    let after = eventually("the card after a metadata read", || {
        rx.try_recv().ok().and_then(|m| match m.event {
            SessionEvent::NowPlaying(n) if n.title.as_deref() == Some("Derezzed") => Some(n),
            _ => None,
        })
    })
    .await;
    assert_eq!(
        after.shuffle,
        Some(true),
        "the metadata response says nothing about shuffle and must not clear it"
    );
    assert_eq!(
        after.repeat,
        Some(castaway_core::RepeatMode::Track),
        "nor about repeat"
    );
}
