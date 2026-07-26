//! The record server: answers what a phone asks about us.

use bytes::{BufMut, Bytes, BytesMut};

use crate::element::DataElement;
use crate::error::SdpError;
use crate::pdu::{error_code, AttributeRange, Continuation, SdpRequest, SdpResponse};
use crate::record::ServiceRecord;
use crate::uuid::Uuid;

/// Serves a fixed set of service records.
///
/// Stateless across continuations by construction: the continuation token *is* the offset
/// into a response the server rebuilds each time. That avoids per-peer session state and
/// the stale-token bugs that come with it, at the cost of re-encoding a few hundred bytes
/// — which is free compared to the round trip that prompted it.
#[derive(Debug, Default)]
pub struct SdpServer {
    records: Vec<ServiceRecord>,
}

impl SdpServer {
    /// A server with no records.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a record.
    pub fn add(&mut self, record: ServiceRecord) {
        self.records.push(record);
    }

    /// Publish a record, builder-style.
    #[must_use]
    pub fn with(mut self, record: ServiceRecord) -> Self {
        self.add(record);
        self
    }

    /// Every published record.
    #[must_use]
    pub fn records(&self) -> &[ServiceRecord] {
        &self.records
    }

    /// Handle a request PDU and produce the response PDU.
    ///
    /// Malformed input becomes an SDP error response rather than an `Err`: the peer is
    /// waiting on this transaction, and dropping it makes a broken request look like a
    /// dead link.
    #[must_use]
    pub fn handle(&self, request: &[u8]) -> Bytes {
        match SdpRequest::decode(request) {
            Ok(req) => self.dispatch(&req).encode(),
            Err(SdpError::UnsupportedPdu(_)) => SdpResponse::Error {
                tid: peek_tid(request),
                code: error_code::INVALID_REQUEST_SYNTAX,
            }
            .encode(),
            Err(_) => SdpResponse::Error {
                tid: peek_tid(request),
                code: error_code::INVALID_PDU_SIZE,
            }
            .encode(),
        }
    }

    fn dispatch(&self, req: &SdpRequest) -> SdpResponse {
        match req {
            SdpRequest::ServiceSearch {
                tid,
                patterns,
                max_records,
                ..
            } => {
                let handles: Vec<u32> = self
                    .matching(patterns)
                    .filter_map(ServiceRecord::handle)
                    .take(usize::from(*max_records).max(1))
                    .collect();
                SdpResponse::ServiceSearch {
                    tid: *tid,
                    total: u16::try_from(handles.len()).unwrap_or(u16::MAX),
                    handles,
                    cont: Continuation::none(),
                }
            }
            SdpRequest::ServiceAttribute {
                tid,
                handle,
                max_bytes,
                attributes,
                cont,
            } => {
                let Some(record) = self.records.iter().find(|r| r.handle() == Some(*handle)) else {
                    return SdpResponse::Error {
                        tid: *tid,
                        code: error_code::INVALID_RECORD_HANDLE,
                    };
                };
                let full = encode_attributes(record, attributes);
                match slice(&full, *max_bytes, cont) {
                    Some((chunk, next)) => SdpResponse::ServiceAttribute {
                        tid: *tid,
                        attributes: chunk,
                        cont: next,
                    },
                    None => SdpResponse::Error {
                        tid: *tid,
                        code: error_code::INVALID_CONTINUATION_STATE,
                    },
                }
            }
            SdpRequest::ServiceSearchAttribute {
                tid,
                patterns,
                max_bytes,
                attributes,
                cont,
            } => {
                // Concatenate the encoded attribute lists and wrap them, rather than
                // decoding each back into a `DataElement` and re-encoding the outer
                // sequence. That round trip is lossy in exactly the way that matters:
                // decoding turns a pinned 16-bit attribute id into a plain integer, and
                // re-encoding writes it in the narrowest width — undoing the pinning and
                // shifting every element after it.
                let mut body = BytesMut::with_capacity(256);
                for record in self.matching(patterns) {
                    body.extend_from_slice(&encode_attributes(record, attributes));
                }
                let full = wrap_sequence(&body);
                match slice(&full, *max_bytes, cont) {
                    Some((chunk, next)) => SdpResponse::ServiceSearchAttribute {
                        tid: *tid,
                        lists: chunk,
                        cont: next,
                    },
                    None => SdpResponse::Error {
                        tid: *tid,
                        code: error_code::INVALID_CONTINUATION_STATE,
                    },
                }
            }
        }
    }

    /// Records containing *every* UUID in the search pattern.
    ///
    /// The spec says a UUID matches if it appears anywhere in the record, not just in the
    /// class list — peers legitimately search by protocol UUID (`L2CAP`, `AVDTP`) as well
    /// as by service class, and matching only the class list makes those searches
    /// silently return nothing.
    fn matching<'a>(&'a self, patterns: &'a [Uuid]) -> impl Iterator<Item = &'a ServiceRecord> {
        self.records
            .iter()
            .filter(move |r| patterns.iter().all(|p| record_contains(r, *p)))
    }
}

/// Whether `uuid` appears anywhere in the record's attribute values.
fn record_contains(record: &ServiceRecord, uuid: Uuid) -> bool {
    record.iter().any(|(_, v)| element_contains(v, uuid))
}

fn element_contains(element: &DataElement, uuid: Uuid) -> bool {
    match element {
        DataElement::Uuid(u) => u.as_bytes() == uuid.as_bytes(),
        DataElement::Sequence(items) | DataElement::Alternative(items) => {
            items.iter().any(|i| element_contains(i, uuid))
        }
        _ => false,
    }
}

/// Encode the requested attributes of one record as an `[id, value, …]` sequence.
///
/// **Attribute identifiers are always 16-bit unsigned integers**, and that width is not
/// negotiable the way a value's is. Writing them through the ordinary narrowest-width
/// integer encoder emits `0x0000` as a *one-byte* uint, which shifts every following
/// element by one and turns the whole record into garbage — a real client reads the
/// record handle as `0x1` and the rest as `0xffffffff`. Found by pointing BlueZ's
/// `sdptool` at us; our own parser was lenient enough not to notice.
fn encode_attributes(record: &ServiceRecord, wanted: &[AttributeRange]) -> Bytes {
    /// Unsigned integer, size index 1 — two bytes.
    const UINT_16: u8 = (1 << 3) | 1;

    let mut body = BytesMut::with_capacity(64);
    for (id, value) in record.iter() {
        if wanted.iter().any(|r| r.contains(id)) {
            body.put_u8(UINT_16);
            body.put_u16(id);
            value.encode(&mut body);
        }
    }
    wrap_sequence(&body)
}

/// Wrap an encoded body in a data-element sequence header.
fn wrap_sequence(body: &[u8]) -> Bytes {
    /// Sequence with a one-byte length.
    const SEQ_U8: u8 = (6 << 3) | 5;
    /// Sequence with a two-byte length.
    const SEQ_U16: u8 = (6 << 3) | 6;

    let mut out = BytesMut::with_capacity(body.len() + 3);
    if let Ok(len) = u8::try_from(body.len()) {
        out.put_u8(SEQ_U8);
        out.put_u8(len);
    } else {
        out.put_u8(SEQ_U16);
        out.put_u16(u16::try_from(body.len()).unwrap_or(u16::MAX));
    }
    out.extend_from_slice(body);
    out.freeze()
}

/// Cut `full` down to what fits, returning the chunk and the token for the rest.
///
/// Returns `None` if the peer echoed a token we never issued — which the caller turns
/// into an `INVALID_CONTINUATION_STATE` rather than silently restarting, because
/// restarting would loop the peer forever.
fn slice(full: &Bytes, max_bytes: u16, cont: &Continuation) -> Option<(Bytes, Continuation)> {
    let start = if cont.is_more() {
        usize::from(cont.offset()?)
    } else {
        0
    };
    if start > full.len() {
        return None;
    }
    // Leave room for the byte-count field and the continuation token itself; a chunk
    // sized to the raw ceiling produces a PDU one or two bytes over it, which strict
    // peers reject.
    let budget = usize::from(max_bytes).saturating_sub(8).max(1);
    let end = (start + budget).min(full.len());
    let chunk = full.slice(start..end);
    let next = if end < full.len() {
        Continuation::at(u16::try_from(end).unwrap_or(u16::MAX))
    } else {
        Continuation::none()
    };
    Some((chunk, next))
}

fn peek_tid(buf: &[u8]) -> u16 {
    if buf.len() >= 3 {
        u16::from_be_bytes([buf[1], buf[2]])
    } else {
        0
    }
}

/// Reassemble the attribute-lists element from a response that may have been split.
///
/// # Errors
/// [`SdpError::Truncated`] or [`SdpError::BadElement`] if the reassembled bytes are not a
/// valid element sequence.
pub fn parse_records(lists: &[u8]) -> Result<Vec<ServiceRecord>, SdpError> {
    let (element, _) = DataElement::decode(lists)?;
    let Some(records) = element.as_sequence() else {
        return Err(SdpError::Missing("attribute lists sequence"));
    };
    let mut out = Vec::new();
    for record in records {
        let Some(items) = record.as_sequence() else {
            continue;
        };
        let mut rec = ServiceRecord::new();
        for pair in items.chunks(2) {
            let [id, value] = pair else { continue };
            let Some(id) = id.as_uint().and_then(|v| u16::try_from(v).ok()) else {
                continue;
            };
            rec = rec.with(id, value.clone());
        }
        out.push(rec);
    }
    Ok(out)
}

/// Concatenate response fragments; a convenience for the continuation loop.
#[must_use]
pub fn join(fragments: &[Bytes]) -> Bytes {
    let mut buf = BytesMut::with_capacity(fragments.iter().map(Bytes::len).sum());
    for f in fragments {
        buf.extend_from_slice(f);
    }
    buf.freeze()
}
