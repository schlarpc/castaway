//! The client side: what we ask a phone about itself.
//!
//! There is exactly one thing we need from the peer's records that no OS stack will give
//! us — the **cover-art PSM**. AVRCP 1.6 publishes it in the Target record's
//! `AdditionalProtocolDescriptorList`, and fetching album art is impossible without it.
//! `bluetoothd` parses that record and never surfaces the field; `obexd` has no BIP
//! client. This module is why album art works here and not there.

use bytes::Bytes;

use crate::error::SdpError;
use crate::pdu::{AttributeRange, Continuation, SdpRequest, SdpResponse};
use crate::record::{attr, ServiceRecord};
use crate::server::{join, parse_records};
use crate::uuid::Uuid;

/// Drives one SDP query across however many continuation round trips it takes.
///
/// Continuation is not optional in practice: a phone's AVRCP Target record with browsing
/// and cover art can exceed a 672-byte MTU, and a client that ignores the token silently
/// truncates the record — losing whichever attribute happened to land past the cut.
#[derive(Debug)]
pub struct Query {
    patterns: Vec<Uuid>,
    attributes: Vec<AttributeRange>,
    max_bytes: u16,
    tid: u16,
    fragments: Vec<Bytes>,
    cont: Continuation,
    done: bool,
}

impl Query {
    /// Ask for `attributes` of every record matching `patterns`.
    #[must_use]
    pub fn new(
        tid: u16,
        patterns: Vec<Uuid>,
        attributes: Vec<AttributeRange>,
        max_bytes: u16,
    ) -> Self {
        Self {
            patterns,
            attributes,
            max_bytes,
            tid,
            fragments: Vec::new(),
            cont: Continuation::none(),
            done: false,
        }
    }

    /// Ask a peer for everything in its AVRCP Target record.
    ///
    /// Requests the whole attribute range rather than just
    /// [`attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST`]: peers vary in where they put the
    /// cover-art stack, and one extra round trip is cheaper than a narrow request that
    /// comes back empty on some handset and takes a day to diagnose.
    #[must_use]
    pub fn avrcp_target(tid: u16) -> Self {
        Self::new(
            tid,
            vec![Uuid::AV_REMOTE_CONTROL_TARGET],
            vec![AttributeRange::Range(0x0000, 0xFFFF)],
            672,
        )
    }

    /// The next request PDU to send, or `None` once the query is complete.
    #[must_use]
    pub fn next_request(&self) -> Option<Bytes> {
        if self.done {
            return None;
        }
        Some(
            SdpRequest::ServiceSearchAttribute {
                tid: self.tid,
                patterns: self.patterns.clone(),
                max_bytes: self.max_bytes,
                attributes: self.attributes.clone(),
                cont: self.cont.clone(),
            }
            .encode(),
        )
    }

    /// Feed a response PDU. Returns `true` when the query is complete.
    ///
    /// # Errors
    /// [`SdpError::PeerError`] if the peer returned an error response, or a parse error
    /// if the PDU is malformed.
    pub fn feed(&mut self, response: &[u8]) -> Result<bool, SdpError> {
        match SdpResponse::decode(response)? {
            SdpResponse::ServiceSearchAttribute { lists, cont, .. } => {
                self.fragments.push(lists);
                if cont.is_more() {
                    self.cont = cont;
                } else {
                    self.done = true;
                }
                Ok(self.done)
            }
            SdpResponse::Error { code, .. } => Err(SdpError::PeerError(code)),
            _ => Err(SdpError::Missing("service search attribute response")),
        }
    }

    /// The records collected, once the query is complete.
    ///
    /// # Errors
    /// A parse error if the reassembled bytes are not a valid attribute-lists sequence.
    pub fn records(&self) -> Result<Vec<ServiceRecord>, SdpError> {
        parse_records(&join(&self.fragments))
    }

    /// The peer's cover-art PSM, if it published one.
    ///
    /// Named by its *stack*, not by its position: the additional descriptor list holds
    /// every extra channel the record offers, and on an iPhone the AVCTP **browsing**
    /// channel is in there and comes first. Taking the first PSM found opens a browsing
    /// channel and then speaks OBEX at it — which fails in a way indistinguishable from
    /// the peer having no cover art at all, and is why an iPhone appeared not to support
    /// it (Q29).
    ///
    /// # Errors
    /// A parse error if the response could not be decoded.
    pub fn cover_art_psm(&self) -> Result<Option<u16>, SdpError> {
        Ok(self.records()?.iter().find_map(|r| {
            r.l2cap_psm_under(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST, Some(Uuid::OBEX))
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::element::DataElement;
    use crate::record::a2dp_sink;
    use crate::server::SdpServer;

    /// Run a query to completion against a server, returning the round-trip count.
    fn run(query: &mut Query, server: &SdpServer) -> usize {
        for trips in 1..=16 {
            let Some(req) = query.next_request() else {
                return trips - 1;
            };
            let rsp = server.handle(&req);
            if query.feed(&rsp).unwrap() {
                return trips;
            }
        }
        panic!("query did not converge");
    }

    fn peer_with_cover_art(psm: u16) -> SdpServer {
        let record = ServiceRecord::new()
            .with(attr::SERVICE_RECORD_HANDLE, DataElement::Uint(0x10000))
            .with(
                attr::SERVICE_CLASS_ID_LIST,
                DataElement::uuid_seq([Uuid::AV_REMOTE_CONTROL_TARGET]),
            )
            .with(
                attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
                DataElement::Sequence(vec![DataElement::Sequence(vec![
                    DataElement::Sequence(vec![
                        DataElement::Uuid(Uuid::L2CAP),
                        DataElement::Uint(u64::from(psm)),
                    ]),
                    DataElement::Sequence(vec![DataElement::Uuid(Uuid::OBEX)]),
                ])]),
            );
        SdpServer::new().with(record)
    }

    #[test]
    fn a_client_finds_the_peers_cover_art_psm() {
        // The whole point of the client half: this PSM is the only route to album art,
        // and it is exactly what BlueZ never hands up.
        let peer = peer_with_cover_art(0x1005);
        let mut q = Query::avrcp_target(1);
        run(&mut q, &peer);
        assert_eq!(q.cover_art_psm().unwrap(), Some(0x1005));
    }

    #[test]
    fn a_query_ignores_a_browsing_channel_published_alongside_the_image_server() {
        // What an iPhone's Target record actually looks like: two extra stacks, browsing
        // first. Reading the list in order gets a browsing PSM, an OBEX CONNECT that is
        // never answered, and a phone that appears not to do cover art.
        let record = ServiceRecord::new()
            .with(attr::SERVICE_RECORD_HANDLE, DataElement::Uint(0x10000))
            .with(
                attr::SERVICE_CLASS_ID_LIST,
                DataElement::uuid_seq([Uuid::AV_REMOTE_CONTROL_TARGET]),
            )
            .with(
                attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
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
        let peer = SdpServer::new().with(record);
        let mut q = Query::avrcp_target(1);
        run(&mut q, &peer);
        assert_eq!(q.cover_art_psm().unwrap(), Some(0x1005));
    }

    #[test]
    fn a_peer_without_cover_art_yields_none_rather_than_an_error() {
        // Plenty of senders don't implement AVRCP 1.6. Text-only now-playing is a
        // degraded result, not a failure.
        let peer = SdpServer::new().with(a2dp_sink(1, "not a phone"));
        let mut q = Query::new(
            1,
            vec![Uuid::AUDIO_SINK],
            vec![AttributeRange::Range(0, 0xFFFF)],
            672,
        );
        run(&mut q, &peer);
        assert_eq!(q.cover_art_psm().unwrap(), None);
    }

    #[test]
    fn a_response_too_large_for_one_pdu_is_reassembled_across_continuations() {
        // Force the split with a tiny ceiling. A client that ignores the continuation
        // token gets a truncated record and loses whichever attribute fell past the cut
        // — here, the PSM itself.
        let peer = peer_with_cover_art(0x1005);
        let mut q = Query::new(
            7,
            vec![Uuid::AV_REMOTE_CONTROL_TARGET],
            vec![AttributeRange::Range(0, 0xFFFF)],
            24,
        );
        let trips = run(&mut q, &peer);
        assert!(trips > 1, "expected the response to be split, took {trips}");
        assert_eq!(q.cover_art_psm().unwrap(), Some(0x1005));
    }

    #[test]
    fn attribute_ids_are_encoded_as_sixteen_bit_integers() {
        // The width is fixed by the spec and is not the value's business. Emitting
        // attribute 0x0000 as a one-byte uint shifts every following element and turns
        // the record into garbage — BlueZ's sdptool read our handle as 0x1.
        let server = SdpServer::new().with(a2dp_sink(0x0001_0000, "x"));
        let mut q = Query::new(
            1,
            vec![Uuid::AUDIO_SINK],
            vec![AttributeRange::Range(0, 0xFFFF)],
            672,
        );
        run(&mut q, &server);

        let records = q.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].handle(),
            Some(0x0001_0000),
            "the record handle must survive the round trip intact"
        );
        assert_eq!(
            records[0].l2cap_psm(attr::PROTOCOL_DESCRIPTOR_LIST),
            Some(0x0019),
            "and so must everything after it"
        );
    }

    #[test]
    fn a_peer_error_response_surfaces_as_a_typed_error() {
        let server = SdpServer::new();
        let mut q = Query::new(1, vec![Uuid::AUDIO_SINK], vec![], 672);
        // A handle-based request against an empty server is refused.
        let req = SdpRequest::ServiceAttribute {
            tid: 1,
            handle: 0xDEAD_BEEF,
            max_bytes: 672,
            attributes: vec![AttributeRange::Single(0)],
            cont: Continuation::none(),
        }
        .encode();
        let rsp = server.handle(&req);
        assert!(matches!(q.feed(&rsp), Err(SdpError::PeerError(_))));
    }
}
