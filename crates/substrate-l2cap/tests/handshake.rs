//! Two multiplexers driven against each other over an in-memory link.
//!
//! This is the tier-1 harness the whole Bluetooth stack leans on (ground rule 6): a real
//! sink and a real peer complete a genuine connect → configure → data → disconnect flow
//! with no controller, no socket and no hardware. Every assertion below is a behaviour a
//! phone would otherwise have to be present to exercise.

#![allow(clippy::unwrap_used)]

use bytes::Bytes;
use substrate_l2cap::{Cid, L2capEvent, L2capPdu, Multiplexer, Psm};

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

    /// The peer connects to `psm` on us; returns (our CID, their CID).
    fn connect(&mut self, psm: Psm) -> (Cid, Cid) {
        let start = self.peer.connect(psm).unwrap();
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
    let events = sink.connect(Psm::AVCTP).unwrap();
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
    let start = link.peer.connect(Psm::RFCOMM).unwrap();
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
