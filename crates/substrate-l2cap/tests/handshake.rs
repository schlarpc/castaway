//! Two multiplexers driven against each other over an in-memory link.
//!
//! This is the tier-1 harness the whole Bluetooth stack leans on (ground rule 6): a real
//! sink and a real peer complete a genuine connect → configure → data → disconnect flow
//! with no controller, no socket and no hardware. Every assertion below is a behaviour a
//! phone would otherwise have to be present to exercise.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use bytes::Bytes;
use substrate_l2cap::signaling::{ConfigOption, Signal};
use substrate_l2cap::{ChannelMode, Cid, L2capEvent, L2capPdu, Multiplexer, Psm};

/// A dynamic PSM of the shape a phone publishes its image server on.
fn cover_art_psm() -> Psm {
    Psm::new(0x1005).unwrap()
}

/// A link that carries PDUs between two multiplexers, returning what each side observed.
struct Link {
    sink: Multiplexer,
    peer: Multiplexer,
}

/// Non-`Send` events — i.e. everything that is a fact rather than an instruction.
fn observations(events: Vec<L2capEvent>) -> Vec<L2capEvent> {
    events
        .into_iter()
        .filter(|e| !matches!(e, L2capEvent::Send(_)))
        .collect()
}

impl Link {
    fn new() -> Self {
        let mut sink = Multiplexer::new(1024);
        sink.listen(Psm::AVDTP);
        sink.listen(Psm::AVCTP);
        Self {
            sink,
            peer: Multiplexer::new(672),
        }
    }

    /// Deliver `events` from whichever side produced them, then keep shuttling until
    /// neither side has anything left to send. Returns everything both sides observed.
    fn settle(
        &mut self,
        from_peer: bool,
        events: Vec<L2capEvent>,
    ) -> (Vec<L2capEvent>, Vec<L2capEvent>) {
        let mut sink_seen = Vec::new();
        let mut peer_seen = Vec::new();
        // FIFO: the link preserves ordering, and L2CAP depends on it — a configuration
        // request addressed to a CID the peer only learns from the connection response
        // cannot legally overtake it.
        let mut queue: std::collections::VecDeque<(bool, L2capPdu)> =
            std::collections::VecDeque::new();
        let mut collect =
            |from_peer: bool,
             events: Vec<L2capEvent>,
             queue: &mut std::collections::VecDeque<(bool, L2capPdu)>| {
                for e in events {
                    match e {
                        L2capEvent::Send(pdu) => queue.push_back((from_peer, pdu)),
                        other => {
                            if from_peer {
                                peer_seen.push(other);
                            } else {
                                sink_seen.push(other);
                            }
                        }
                    }
                }
            };
        collect(from_peer, events, &mut queue);

        // Bounded so a signaling loop fails the test instead of hanging CI.
        for _ in 0..64 {
            let Some((origin, pdu)) = queue.pop_front() else {
                break;
            };
            let out = if origin {
                self.sink.handle_pdu(&pdu).unwrap()
            } else {
                self.peer.handle_pdu(&pdu).unwrap()
            };
            collect(!origin, out, &mut queue);
        }
        assert!(queue.is_empty(), "signaling did not settle");
        (sink_seen, peer_seen)
    }

    /// We connect out to `psm` on the peer in `mode`; returns (our CID, their CID).
    ///
    /// The direction the cover-art fetch actually runs in: the phone publishes an image
    /// server and we are the client.
    fn connect_out(&mut self, psm: Psm, mode: ChannelMode) -> (Cid, Cid) {
        let (_, start) = self.sink.connect_with(psm, mode).unwrap();
        let (sink_seen, peer_seen) = self.settle(false, start);
        let find = |seen: &[L2capEvent]| {
            seen.iter().find_map(|e| match e {
                L2capEvent::ChannelOpen { cid, psm: p, .. } if *p == psm => Some(*cid),
                _ => None,
            })
        };
        (
            find(&sink_seen).expect("we should have opened a channel"),
            find(&peer_seen).expect("the peer should have opened a channel"),
        )
    }

    /// The peer connects to `psm` on us; returns (our CID, their CID).
    fn connect(&mut self, psm: Psm) -> (Cid, Cid) {
        let (_, start) = self.peer.connect(psm).unwrap();
        let (sink_seen, peer_seen) = self.settle(true, start);

        let sink_cid = sink_seen
            .iter()
            .find_map(|e| match e {
                L2capEvent::ChannelOpen { cid, psm: p, .. } if *p == psm => Some(*cid),
                _ => None,
            })
            .expect("sink should have opened a channel");
        let peer_cid = peer_seen
            .iter()
            .find_map(|e| match e {
                L2capEvent::ChannelOpen { cid, psm: p, .. } if *p == psm => Some(*cid),
                _ => None,
            })
            .expect("peer should have opened a channel");
        (sink_cid, peer_cid)
    }
}

#[test]
fn a_full_avdtp_channel_handshake_completes_on_both_sides() {
    // The flow a phone runs the moment it decides to play: connect to AVDTP, configure,
    // and only then be usable. Both ends must reach Open — a channel that is open on one
    // side only is the "connected but the first packet is rejected" failure.
    let mut link = Link::new();
    let (sink_cid, peer_cid) = link.connect(Psm::AVDTP);

    assert!(sink_cid.is_dynamic() && peer_cid.is_dynamic());
    let sink_ch = link.sink.channel(sink_cid).unwrap();
    let peer_ch = link.peer.channel(peer_cid).unwrap();
    assert_eq!(sink_ch.psm, Psm::AVDTP);

    // Each side allocated its own identifier for the same channel, and each knows the
    // other's. These being crossed is *the* classic L2CAP bug.
    //
    // Note the two numbers may well be *equal* — both ends allocate from 0x0040 up, so
    // the first channel on a fresh link is 0x0040 on both sides. That coincidence is
    // exactly why the mapping has to be asserted rather than the values: code that
    // conflates the two CIDs works perfectly on the first channel and breaks on the
    // second, which is the worst possible way for this bug to present.
    assert_eq!(sink_ch.remote_cid, peer_cid);
    assert_eq!(peer_ch.remote_cid, sink_cid);
}

#[test]
fn a_second_channel_uses_a_fresh_cid_on_each_side() {
    // The case that catches conflated CIDs: once the counters have moved, addressing a
    // channel by the wrong side's identifier reaches a different channel or none.
    let mut link = Link::new();
    let (first_sink, first_peer) = link.connect(Psm::AVDTP);
    let (second_sink, second_peer) = link.connect(Psm::AVCTP);

    assert_ne!(first_sink, second_sink);
    assert_ne!(first_peer, second_peer);
    assert_eq!(
        link.sink.channel(second_sink).unwrap().remote_cid,
        second_peer
    );
    assert_eq!(
        link.peer.channel(second_peer).unwrap().remote_cid,
        second_sink
    );
    // …and the first channel's mapping was not disturbed by the second.
    assert_eq!(
        link.sink.channel(first_sink).unwrap().remote_cid,
        first_peer
    );
}

#[test]
fn each_side_learns_the_others_receive_mtu_not_its_own() {
    // We advertise 1024, the peer 672. Our send ceiling is *their* number and vice
    // versa; confusing the two either wastes bandwidth or overflows the peer.
    let mut link = Link::new();
    let (sink_cid, peer_cid) = link.connect(Psm::AVDTP);

    let sink_ch = link.sink.channel(sink_cid).unwrap();
    assert_eq!(sink_ch.local_mtu, 1024);
    assert_eq!(sink_ch.remote_mtu, 672, "we may only send what they accept");

    let peer_ch = link.peer.channel(peer_cid).unwrap();
    assert_eq!(peer_ch.local_mtu, 672);
    assert_eq!(peer_ch.remote_mtu, 1024);
}

#[test]
fn data_flows_only_after_configuration_and_reaches_the_far_side() {
    let mut link = Link::new();
    let (sink_cid, peer_cid) = link.connect(Psm::AVDTP);

    let payload = Bytes::from_static(&[0x10, 0x01, 0x02, 0x03]);
    let sends = link.peer.send(peer_cid, payload.clone()).unwrap();
    let (sink_seen, _) = link.settle(true, sends);

    assert_eq!(
        sink_seen,
        vec![L2capEvent::Data {
            cid: sink_cid,
            psm: Psm::AVDTP,
            payload,
        }]
    );
}

#[test]
fn sending_before_the_channel_is_open_is_refused() {
    // Half-configured channels must not carry data. The state machine says no rather
    // than emitting a PDU the peer will drop without telling anyone.
    let mut sink = Multiplexer::new(672);
    sink.listen(Psm::AVDTP);
    let (_, events) = sink.connect(Psm::AVCTP).unwrap();
    let L2capEvent::Send(pdu) = &events[0] else {
        panic!("expected a connection request");
    };
    let sig = substrate_l2cap::Signal::decode_all(&pdu.payload).unwrap();
    let substrate_l2cap::Signal::ConnectionRequest { source_cid, .. } = sig[0] else {
        panic!("expected a connection request");
    };
    assert!(sink.send(source_cid, Bytes::from_static(&[1])).is_err());
}

#[test]
fn an_unregistered_psm_is_refused_rather_than_ignored() {
    // RFCOMM is a PSM we deliberately don't serve. The peer must get a definite
    // "not supported" instead of a timeout.
    let mut link = Link::new();
    let (_, start) = link.peer.connect(Psm::RFCOMM).unwrap();
    let (_, peer_seen) = link.settle(true, start);

    assert_eq!(
        peer_seen,
        vec![L2capEvent::ConnectFailed {
            psm: Psm::RFCOMM,
            result: substrate_l2cap::ConnectionResult::PsmNotSupported,
        }]
    );
}

#[test]
fn two_services_run_on_independent_channels() {
    // A2DP and AVRCP are two L2CAP channels on the same ACL link, and the AVCTP one
    // typically comes up *after* audio is already flowing — the reason core publishes
    // the control surface as a separate event.
    let mut link = Link::new();
    let (avdtp_sink, avdtp_peer) = link.connect(Psm::AVDTP);
    let (avctp_sink, avctp_peer) = link.connect(Psm::AVCTP);

    assert_ne!(avdtp_sink, avctp_sink);
    assert_eq!(link.sink.channel(avdtp_sink).unwrap().psm, Psm::AVDTP);
    assert_eq!(link.sink.channel(avctp_sink).unwrap().psm, Psm::AVCTP);

    // Data on one lands on that one only.
    let sends = link
        .peer
        .send(avctp_peer, Bytes::from_static(&[0xAA]))
        .unwrap();
    let (sink_seen, _) = link.settle(true, sends);
    assert_eq!(
        sink_seen,
        vec![L2capEvent::Data {
            cid: avctp_sink,
            psm: Psm::AVCTP,
            payload: Bytes::from_static(&[0xAA]),
        }]
    );
    assert!(link.peer.channel(avdtp_peer).is_some());
}

#[test]
fn a_disconnect_closes_the_channel_on_both_sides() {
    let mut link = Link::new();
    let (sink_cid, peer_cid) = link.connect(Psm::AVDTP);

    let start = link.peer.disconnect(peer_cid).unwrap();
    let (sink_seen, peer_seen) = link.settle(true, start);

    assert!(sink_seen.contains(&L2capEvent::ChannelClosed {
        cid: sink_cid,
        psm: Psm::AVDTP,
    }));
    assert!(peer_seen.contains(&L2capEvent::ChannelClosed {
        cid: peer_cid,
        psm: Psm::AVDTP,
    }));
    assert!(link.sink.channel(sink_cid).is_none());
    assert!(link.peer.channel(peer_cid).is_none());
}

#[test]
fn a_dropped_link_closes_every_channel_on_it() {
    // The phone walks out of the room: no disconnection handshake, just a dead link.
    // Every channel has to be reaped or the next session inherits stale state.
    let mut link = Link::new();
    let (avdtp, _) = link.connect(Psm::AVDTP);
    let (avctp, _) = link.connect(Psm::AVCTP);

    let closed = observations(link.sink.link_down());
    assert_eq!(closed.len(), 2);
    assert!(closed.contains(&L2capEvent::ChannelClosed {
        cid: avdtp,
        psm: Psm::AVDTP
    }));
    assert!(closed.contains(&L2capEvent::ChannelClosed {
        cid: avctp,
        psm: Psm::AVCTP
    }));
    assert_eq!(link.sink.channels().count(), 0);
}

#[test]
fn an_sdu_larger_than_the_peers_mtu_is_refused_not_truncated() {
    // Basic mode has no segmentation, so an oversized SDU is a protocol error the peer
    // drops silently. Refusing locally turns a mystery into a typed error.
    let mut link = Link::new();
    let (sink_cid, _) = link.connect(Psm::AVDTP);
    let too_big = Bytes::from(vec![0u8; 673]); // peer advertised 672
    assert!(link.sink.send(sink_cid, too_big).is_err());
    assert!(link
        .sink
        .send(sink_cid, Bytes::from(vec![0u8; 672]))
        .is_ok());
}

#[test]
fn data_for_an_unknown_channel_is_an_error() {
    let mut link = Link::new();
    let stray = L2capPdu::new(Cid::new(0x00ff), Bytes::from_static(&[1, 2]));
    assert!(link.sink.handle_pdu(&stray).is_err());
}

#[test]
fn a_cover_art_channel_negotiates_enhanced_retransmission_end_to_end() {
    // The channel Q29 was blocked on. GOEP 2.0 requires ERTM for the OBEX transfer, so a
    // channel that quietly settles into basic mode is a channel the peer will not serve
    // an image over — both ends have to *agree* on ERTM, not merely tolerate it.
    let mut link = Link::new();
    link.peer
        .listen_with(cover_art_psm(), ChannelMode::EnhancedRetransmission);
    let (sink_cid, peer_cid) =
        link.connect_out(cover_art_psm(), ChannelMode::EnhancedRetransmission);

    let sink_ch = link.sink.channel(sink_cid).unwrap();
    let peer_ch = link.peer.channel(peer_cid).unwrap();
    assert_eq!(sink_ch.mode, ChannelMode::EnhancedRetransmission);
    assert_eq!(peer_ch.mode, ChannelMode::EnhancedRetransmission);
    // The frame size we segment against is the peer's *receive* capability, not our own.
    assert!(sink_ch.parameters.send_mps <= peer_ch.local_mtu);
    assert!(sink_ch.parameters.send_window >= 1);
}

#[test]
fn an_object_larger_than_one_frame_crosses_an_ertm_channel_intact() {
    // A 200x200 thumbnail is several kilobytes and no L2CAP frame is: without
    // segmentation the cover-art path tops out at one packet, which is the size of
    // nothing worth showing.
    let mut link = Link::new();
    link.peer
        .listen_with(cover_art_psm(), ChannelMode::EnhancedRetransmission);
    let (_, peer_cid) = link.connect_out(cover_art_psm(), ChannelMode::EnhancedRetransmission);

    let image: Vec<u8> = (0..3000).map(|i| u8::try_from(i % 251).unwrap()).collect();
    let sends = link
        .peer
        .send(peer_cid, Bytes::from(image.clone()))
        .unwrap();
    let (sink_seen, _) = link.settle(true, sends);

    let delivered: Vec<Bytes> = sink_seen
        .into_iter()
        .filter_map(|e| match e {
            L2capEvent::Data { payload, .. } => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered.len(),
        1,
        "the SDU must arrive once, not as its segments"
    );
    assert_eq!(&delivered[0][..], &image[..]);
}

#[test]
fn a_peer_that_only_speaks_basic_mode_gets_a_counter_proposal_and_a_channel_anyway() {
    // Advertising ERTM support must not make us refuse peers that do not have it. A
    // counter-proposal naming basic mode is how the spec says "not that one, this one",
    // and it is the difference between falling back and hanging up.
    let mut link = Link::new();
    link.peer.listen(cover_art_psm()); // basic only
    let (sink_cid, peer_cid) =
        link.connect_out(cover_art_psm(), ChannelMode::EnhancedRetransmission);

    assert_eq!(
        link.sink.channel(sink_cid).unwrap().mode,
        ChannelMode::Basic,
        "we should have come down to the mode the peer offered"
    );
    assert_eq!(
        link.peer.channel(peer_cid).unwrap().mode,
        ChannelMode::Basic
    );

    // …and it carries data, which is the whole point of falling back rather than failing.
    let payload = Bytes::from_static(b"GET / OBEX");
    let sends = link.peer.send(peer_cid, payload.clone()).unwrap();
    let (sink_seen, _) = link.settle(true, sends);
    assert!(sink_seen.iter().any(|e| matches!(
        e,
        L2capEvent::Data { payload: got, .. } if got == &payload
    )));
}

#[test]
fn a_phone_that_asks_for_retransmission_on_audio_is_brought_back_to_basic_mode() {
    // The interop risk that comes free with advertising ERTM: a sender that sees the bit
    // may well propose the mode for AVDTP too. Our audio channels are registered basic —
    // hardware-proven, and A2DP has no use for retransmission — so the answer has to be a
    // counter-proposal the sender can act on, not a refusal that costs us the session.
    let mut link = Link::new();
    let (_, start) = link
        .peer
        .connect_with(Psm::AVDTP, ChannelMode::EnhancedRetransmission)
        .unwrap();
    let (sink_seen, peer_seen) = link.settle(true, start);

    let opened = |seen: &[L2capEvent]| {
        seen.iter().find_map(|e| match e {
            L2capEvent::ChannelOpen { cid, psm, .. } if *psm == Psm::AVDTP => Some(*cid),
            _ => None,
        })
    };
    let sink_cid = opened(&sink_seen).expect("the audio channel must still open");
    let peer_cid = opened(&peer_seen).expect("and on the sender's side too");
    assert_eq!(
        link.sink.channel(sink_cid).unwrap().mode,
        ChannelMode::Basic
    );
    assert_eq!(
        link.peer.channel(peer_cid).unwrap().mode,
        ChannelMode::Basic,
        "the sender should have come down to the mode we serve audio in"
    );
}

#[test]
fn the_extended_features_mask_advertises_retransmission() {
    // The bit that decides whether a peer bothers proposing ERTM at all. Answering zero
    // here is what left cover art unreachable no matter what the layers above did.
    let mut sink = Multiplexer::new(672);
    let request = L2capPdu::new(
        Cid::SIGNALING,
        substrate_l2cap::Signal::InformationRequest {
            id: 1,
            info_type: 0x0002,
        }
        .encode()
        .unwrap(),
    );
    let events = sink.handle_pdu(&request).unwrap();
    let L2capEvent::Send(pdu) = &events[0] else {
        panic!("expected a reply");
    };
    let sigs = substrate_l2cap::Signal::decode_all(&pdu.payload).unwrap();
    let substrate_l2cap::Signal::InformationResponse { data, result, .. } = &sigs[0] else {
        panic!("expected an information response");
    };
    assert_eq!(*result, 0x0000, "the request must be answered, not refused");
    let mask = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    assert_ne!(mask & (1 << 3), 0, "bit 3 is enhanced retransmission mode");
    assert_ne!(mask & (1 << 5), 0, "bit 5 is the fcs option");
}

#[test]
fn an_ertm_channel_recovers_a_lost_frame_rather_than_losing_the_object() {
    // What ERTM is *for*. Drop one segment on the floor and the object still arrives —
    // in basic mode the same loss is a truncated JPEG nobody can decode and nobody can
    // explain.
    let mut link = Link::new();
    link.peer
        .listen_with(cover_art_psm(), ChannelMode::EnhancedRetransmission);
    let (_, peer_cid) = link.connect_out(cover_art_psm(), ChannelMode::EnhancedRetransmission);

    let object: Vec<u8> = (0..2000).map(|i| u8::try_from(i % 251).unwrap()).collect();
    let sends = link
        .peer
        .send(peer_cid, Bytes::from(object.clone()))
        .unwrap();

    // Deliver everything except the second frame, which the radio ate.
    let mut frames: Vec<L2capPdu> = sends
        .into_iter()
        .filter_map(|e| match e {
            L2capEvent::Send(pdu) => Some(pdu),
            _ => None,
        })
        .collect();
    assert!(frames.len() > 2, "the object must actually be segmented");
    let lost = frames.remove(1);

    drop(lost);

    let mut delivered: Vec<Bytes> = Vec::new();
    for pdu in frames {
        let (sink_seen, _) = link.settle(true, vec![L2capEvent::Send(pdu)]);
        delivered.extend(sink_seen.into_iter().filter_map(|e| match e {
            L2capEvent::Data { payload, .. } => Some(payload),
            _ => None,
        }));
    }

    // The gap makes the receiver reject, the rejection makes the sender replay, and the
    // object arrives whole — once, not as a truncated prefix and not twice.
    assert_eq!(delivered.len(), 1, "the object must survive the loss");
    assert_eq!(&delivered[0][..], &object[..]);
}

// --- the paths a real phone exercises and the happy handshake never does ---

/// Drive `mux` forward `by` and collect what it wants sent.
fn advance(mux: &mut Multiplexer, by: Duration) -> Vec<L2capEvent> {
    mux.tick(by)
}

#[test]
fn a_peer_that_never_answers_is_given_up_on_rather_than_waited_on_forever() {
    // Nothing timed signalling requests at all, so a ConnectionRequest the peer simply
    // ignored left the channel in WaitConnectRsp for the life of the link: no
    // retransmission, no ChannelClosed, the CID never freed, the caller never told. That
    // is a hang, not a failure — and it is what left cover art and the outbound AVRCP
    // channel permanently dead on a phone that ignored our connect.
    let mut mux = Multiplexer::new(672);
    let (_cid, events) = mux.connect(Psm::SDP).expect("dial");
    assert_eq!(events.len(), 1, "the connection request");

    // The spec wants at least one retransmission before giving up: a lost packet is far
    // more likely than a peer that will never answer.
    let mut retransmissions = 0;
    let mut closed = None;
    for _ in 0..12 {
        let Some(due) = mux.next_timeout() else {
            break;
        };
        for event in advance(&mut mux, due) {
            match event {
                L2capEvent::Send(_) => retransmissions += 1,
                L2capEvent::ChannelClosed { psm, .. } => closed = Some(psm),
                _ => {}
            }
        }
        if closed.is_some() {
            break;
        }
    }
    assert!(retransmissions >= 1, "it must retry before giving up");
    assert_eq!(closed, Some(Psm::SDP), "and then tell the caller");
    assert!(
        mux.next_timeout().is_none(),
        "a channel it gave up on must not keep a timer running"
    );
}

#[test]
fn an_answer_stops_the_timer() {
    // The other half: a channel that is answered normally must not be torn down by a
    // timer that nobody cancelled.
    let mut listener = Multiplexer::new(672);
    listener.listen(Psm::SDP);
    let mut dialler = Multiplexer::new(672);
    let (_cid, events) = dialler.connect(Psm::SDP).expect("dial");

    // Hand the request to the listener and its reply back.
    for event in events {
        if let L2capEvent::Send(pdu) = event {
            for reply in listener
                .handle_pdu(&pdu)
                .expect("listener handles the request")
            {
                if let L2capEvent::Send(pdu) = reply {
                    let _ = dialler.handle_pdu(&pdu);
                }
            }
        }
    }
    // A configuration request is now outstanding, but the *connection* timer is gone.
    // Either way, nothing should have been torn down.
    assert!(
        !matches!(
            dialler.tick(Duration::from_millis(1)).as_slice(),
            [L2capEvent::ChannelClosed { .. }, ..]
        ),
        "an answered request must not expire"
    );
}

#[test]
fn one_unknown_command_does_not_discard_the_ones_packed_with_it() {
    // The spec allows several commands in one C-frame and real stacks use it. Failing the
    // whole frame threw away well-formed commands alongside the bad one, and answered the
    // peer with silence where a Command Reject is required — so a phone that packs
    // something we do not implement (Create Channel, Move Channel, anything future) with
    // its ConnectionRequest never got a channel and never learned why.
    let mut mux = Multiplexer::new(672);
    mux.listen(Psm::AVDTP);

    // `0x0C` is Create Channel: a real code, and one we do not implement.
    let unknown: [u8; 4] = [0x0C, 0x42, 0x00, 0x00];
    let connect = Signal::ConnectionRequest {
        id: 0x43,
        psm: Psm::AVDTP,
        source_cid: Cid::new(0x0041),
    }
    .encode()
    .expect("encode");

    let mut frame = Vec::from(unknown);
    frame.extend_from_slice(&connect);
    let events = mux
        .handle_pdu(&L2capPdu::new(Cid::SIGNALING, Bytes::from(frame)))
        .expect("a bad command must not fail the whole frame");

    let sent: Vec<Signal> = events
        .iter()
        .filter_map(|e| match e {
            L2capEvent::Send(pdu) => Some(Signal::decode_all(&pdu.payload).expect("decodes")),
            _ => None,
        })
        .flatten()
        .collect();

    assert!(
        sent.iter().any(|s| matches!(
            s,
            Signal::CommandReject { id: 0x42, reason, .. } if *reason == 0x0000
        )),
        "the unknown command is refused, by id: {sent:?}"
    );
    assert!(
        sent.iter()
            .any(|s| matches!(s, Signal::ConnectionResponse { id: 0x43, .. })),
        "and the good command packed with it is still answered: {sent:?}"
    );
}

#[test]
fn a_rejected_command_closes_the_channel_instead_of_waiting_out_the_timer() {
    // Inbound Command Reject was swallowed, so if a phone refused our configuration
    // request we waited for an answer that was never coming.
    let mut mux = Multiplexer::new(672);
    let (cid, events) = mux.connect(Psm::SDP).expect("dial");
    let id = match events.first() {
        Some(L2capEvent::Send(pdu)) => {
            match Signal::decode_all(&pdu.payload).expect("decodes").first() {
                Some(Signal::ConnectionRequest { id, .. }) => *id,
                other => panic!("expected a connection request, got {other:?}"),
            }
        }
        other => panic!("expected a send, got {other:?}"),
    };

    let reject = Signal::CommandReject {
        id,
        reason: 0x0000,
        data: Bytes::new(),
    }
    .encode()
    .expect("encode");
    let out = mux
        .handle_pdu(&L2capPdu::new(Cid::SIGNALING, reject))
        .expect("a reject is not a parse failure");
    assert!(
        out.iter()
            .any(|e| matches!(e, L2capEvent::ChannelClosed { cid: c, .. } if *c == cid)),
        "the channel we were dialling must be closed: {out:?}"
    );
}

#[test]
fn random_bytes_into_the_signalling_channel_do_not_panic() {
    // Cheap robustness sweep. A malformed PDU from a phone must be an error or a reject,
    // never a panic — a panic here takes the whole Bluetooth actor down with it.
    let mut mux = Multiplexer::new(672);
    mux.listen(Psm::AVDTP);
    let mut seed = 0x1234_5678_u32;
    for _ in 0..2000 {
        let mut bytes = Vec::new();
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = ((seed >> 16) as usize) % 40;
        for _ in 0..len {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            // Truncation is the point: we want arbitrary bytes.
            bytes.push(((seed >> 16) & 0xFF) as u8);
        }
        let _ = mux.handle_pdu(&L2capPdu::new(Cid::SIGNALING, Bytes::from(bytes)));
    }
}

#[test]
fn a_configuration_split_across_requests_does_not_open_the_channel_early() {
    // Bit 0 of the flags is "more options follow". It was destructured away, so a partial
    // option list was answered Success with C=0 and the channel opened while the peer was
    // still describing it — both ends then believing different things about a channel
    // that is nominally up, which shows as "connects, then the first data PDU is dropped".
    let mut mux = Multiplexer::new(672);
    mux.listen(Psm::AVDTP);

    // Connect first, so there is a channel to configure.
    let connect = Signal::ConnectionRequest {
        id: 1,
        psm: Psm::AVDTP,
        source_cid: Cid::new(0x0041),
    }
    .encode()
    .expect("encode");
    let events = mux
        .handle_pdu(&L2capPdu::new(Cid::SIGNALING, connect))
        .expect("connect");
    let ours = events
        .iter()
        .filter_map(|e| match e {
            L2capEvent::Send(pdu) => Signal::decode_all(&pdu.payload).ok(),
            _ => None,
        })
        .flatten()
        .find_map(|s| match s {
            Signal::ConnectionResponse { dest_cid, .. } => Some(dest_cid),
            _ => None,
        })
        .expect("a connection response");

    // First half of the configuration, flagged as continued.
    let partial = Signal::ConfigurationRequest {
        id: 2,
        dest_cid: ours,
        flags: 0x0001,
        options: vec![ConfigOption::Mtu(512)],
    }
    .encode()
    .expect("encode");
    let events = mux
        .handle_pdu(&L2capPdu::new(Cid::SIGNALING, partial))
        .expect("partial config");

    let replies: Vec<Signal> = events
        .iter()
        .filter_map(|e| match e {
            L2capEvent::Send(pdu) => Signal::decode_all(&pdu.payload).ok(),
            _ => None,
        })
        .flatten()
        .collect();
    let echoed = replies
        .iter()
        .find_map(|s| match s {
            Signal::ConfigurationResponse { flags, .. } => Some(*flags),
            _ => None,
        })
        .expect("a configuration response");
    assert_eq!(
        echoed & 0x0001,
        0x0001,
        "the continuation flag must be echoed so the peer knows we followed: {replies:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, L2capEvent::ChannelOpen { .. })),
        "the channel must not open while the peer is still describing it"
    );

    // The rest, C=0 — now it may complete.
    let rest = Signal::ConfigurationRequest {
        id: 3,
        dest_cid: ours,
        flags: 0,
        options: vec![],
    }
    .encode()
    .expect("encode");
    let events = mux
        .handle_pdu(&L2capPdu::new(Cid::SIGNALING, rest))
        .expect("final config");
    assert!(
        events.iter().any(|e| matches!(e, L2capEvent::Send(_))),
        "the final request is still answered: {events:?}"
    );
}

// --- the twelve-second fuse ---

/// Advance both ends of a link in one-second steps, delivering anything either produces,
/// and return everything they observed along the way.
///
/// A second at a time rather than `next_timeout()` because the point of these tests is what
/// happens when *nothing* is due: a channel with no timer running must survive an arbitrary
/// amount of wall clock, and the bug they pin was invisible to any harness that only
/// advanced to the next scheduled event on a channel it had already given up on.
fn run_for(link: &mut Link, seconds: u64) -> (Vec<L2capEvent>, Vec<L2capEvent>) {
    let (mut sink_seen, mut peer_seen) = (Vec::new(), Vec::new());
    for _ in 0..seconds {
        let sink_events = link.sink.tick(Duration::from_secs(1));
        let (s, p) = link.settle(false, sink_events);
        sink_seen.extend(s);
        peer_seen.extend(p);
        let peer_events = link.peer.tick(Duration::from_secs(1));
        let (s, p) = link.settle(true, peer_events);
        sink_seen.extend(s);
        peer_seen.extend(p);
    }
    (sink_seen, peer_seen)
}

#[test]
fn an_open_channel_survives_far_longer_than_the_response_timeout() {
    // The bug, exactly as the logs recorded it. `ConfigurationResponse` was the one answer
    // that did not retire the request it answered, so every channel we ever configured sat
    // on a response timer that could never be satisfied: re-proposed at four seconds,
    // re-proposed at eight, and torn down by *us* at twelve.
    //
    // On the wall panel that is an iPhone twelve seconds into a track having its AVDTP
    // signalling, AVDTP media and AVCTP channels all disconnected out from under it by the
    // receiver, and then hanging up the ACL link a second later with reason 0x13 — which
    // reads, from our side, as "the phone dropped us".
    //
    // Twenty seconds is well past the give-up point and past two more RTX periods after it.
    let mut link = Link::new();
    let (sink_cid, _) = link.connect(Psm::AVDTP);
    let (sink_avctp, _) = link.connect(Psm::AVCTP);

    let (sink_seen, peer_seen) = run_for(&mut link, 20);

    assert!(
        sink_seen.is_empty(),
        "an idle open channel must produce nothing at all: {sink_seen:?}"
    );
    assert!(
        peer_seen.is_empty(),
        "and the peer must see nothing either: {peer_seen:?}"
    );
    assert!(
        link.sink.channel(sink_cid).is_some() && link.sink.channel(sink_avctp).is_some(),
        "both channels must still exist"
    );
    assert_eq!(
        link.sink.next_timeout(),
        None,
        "a settled channel must not be waiting on time at all"
    );
}

#[test]
fn a_configured_channel_still_carries_data_after_the_timeout_would_have_fired() {
    // The failure was not merely bookkeeping: audio stops. This is the same twenty seconds
    // with the thing that actually matters asserted at the end of it.
    let mut link = Link::new();
    let (sink_cid, peer_cid) = link.connect(Psm::AVDTP);
    run_for(&mut link, 20);

    let payload = Bytes::from_static(&[0x80, 0x60, 0x01, 0x02]);
    let sends = link.peer.send(peer_cid, payload.clone()).expect("send");
    let (sink_seen, _) = link.settle(true, sends);
    assert_eq!(
        sink_seen,
        vec![L2capEvent::Data {
            cid: sink_cid,
            psm: Psm::AVDTP,
            payload,
        }],
        "media must still arrive twenty seconds in"
    );
}

#[test]
fn a_cover_art_channel_mid_fetch_does_not_take_the_audio_channels_with_it() {
    // Requirement (2) as a test: album art is decoration, and no failure of it may cost the
    // link its audio. The shape is the one in the log — AVDTP and AVCTP up, an ERTM channel
    // out to the phone's image server, a request sent on it that is never answered — and
    // the assertion is that the audio channels are untouched however that ends.
    let mut link = Link::new();
    let (sink_avdtp, peer_avdtp) = link.connect(Psm::AVDTP);
    let (sink_avctp, _) = link.connect(Psm::AVCTP);
    link.peer
        .listen_with(cover_art_psm(), ChannelMode::EnhancedRetransmission);
    let (sink_art, peer_art) =
        link.connect_out(cover_art_psm(), ChannelMode::EnhancedRetransmission);

    // An OBEX GET that goes out and is never answered — delivered nowhere, so no
    // acknowledgement comes back. This is the state the image channel was in at the moment
    // the phone gave up on it.
    let get = Bytes::from_static(&[0x83, 0x00, 0x08, 0xCB, 0, 0, 0, 1]);
    for event in link.sink.send(sink_art, get).expect("the get goes out") {
        drop(event);
    }
    // …and then the responder closes the image session mid-fetch, which is what the log
    // records the iPhone doing two seconds later.
    let teardown = link.peer.disconnect(peer_art).expect("the peer hangs up");
    let (sink_seen, _) = link.settle(true, teardown);
    assert!(
        sink_seen.iter().any(|e| matches!(
            e,
            L2capEvent::ChannelClosed { cid, .. } if *cid == sink_art
        )),
        "the image channel really did close: {sink_seen:?}"
    );

    // Long past the point at which every channel used to be torn down.
    let (sink_seen, _) = run_for(&mut link, 20);

    let closed_audio: Vec<&L2capEvent> = sink_seen
        .iter()
        .filter(|e| {
            matches!(
                e,
                L2capEvent::ChannelClosed { psm, .. } if *psm == Psm::AVDTP || *psm == Psm::AVCTP
            )
        })
        .collect();
    assert!(
        closed_audio.is_empty(),
        "a cover-art failure must not close an audio channel: {closed_audio:?}"
    );
    assert!(
        link.sink.channel(sink_avdtp).is_some() && link.sink.channel(sink_avctp).is_some(),
        "and both audio channels must still be open"
    );
    assert!(
        link.sink.channel(sink_art).is_none(),
        "while the image channel, which really did fail, stays gone"
    );

    // The thing the panel is for: audio still plays.
    let payload = Bytes::from_static(&[0x80, 0x60, 0xAA, 0xBB]);
    let sends = link.peer.send(peer_avdtp, payload.clone()).expect("send");
    let (sink_seen, _) = link.settle(true, sends);
    assert_eq!(
        sink_seen,
        vec![L2capEvent::Data {
            cid: sink_avdtp,
            psm: Psm::AVDTP,
            payload,
        }]
    );
}

#[test]
fn a_channel_that_never_finishes_configuring_is_still_given_up_on() {
    // The other side of the fix: retiring timers must not blunt the timer. A peer that
    // answers the connection request and then goes silent still has to be abandoned in
    // bounded time, or the CID leaks and the caller waits forever.
    let mut listener = Multiplexer::new(672);
    listener.listen(Psm::SDP);
    let mut dialler = Multiplexer::new(672);
    let (_, events) = dialler.connect(Psm::SDP).expect("dial");

    // The listener answers the connection request only; its configuration request and our
    // own proposal are both dropped on the floor from here on.
    for event in events {
        if let L2capEvent::Send(pdu) = event {
            for reply in listener.handle_pdu(&pdu).expect("connect") {
                if let L2capEvent::Send(pdu) = reply {
                    if let Ok(signals) = Signal::decode_all(&pdu.payload) {
                        if signals
                            .iter()
                            .any(|s| matches!(s, Signal::ConnectionResponse { .. }))
                        {
                            let _ = dialler.handle_pdu(&pdu);
                        }
                    }
                }
            }
        }
    }

    let mut closed = None;
    for _ in 0..40 {
        for event in dialler.tick(Duration::from_secs(1)) {
            if let L2capEvent::ChannelClosed { psm, .. } = event {
                closed = Some(psm);
            }
        }
        if closed.is_some() {
            break;
        }
    }
    assert_eq!(
        closed,
        Some(Psm::SDP),
        "a configuration nobody answers must still fail the channel"
    );
}
