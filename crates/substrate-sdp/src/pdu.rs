//! SDP transaction PDUs.

use bytes::{BufMut, Bytes, BytesMut};

use crate::element::DataElement;
use crate::error::SdpError;
use crate::uuid::Uuid;

/// PDU identifiers.
pub mod pdu_id {
    /// Error response.
    pub const ERROR_RESPONSE: u8 = 0x01;
    /// Service search request.
    pub const SERVICE_SEARCH_REQUEST: u8 = 0x02;
    /// Service search response.
    pub const SERVICE_SEARCH_RESPONSE: u8 = 0x03;
    /// Service attribute request.
    pub const SERVICE_ATTRIBUTE_REQUEST: u8 = 0x04;
    /// Service attribute response.
    pub const SERVICE_ATTRIBUTE_RESPONSE: u8 = 0x05;
    /// Service search attribute request — the one phones actually use.
    pub const SERVICE_SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
    /// Service search attribute response.
    pub const SERVICE_SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;
}

/// Error codes an error response can carry.
pub mod error_code {
    /// Unsupported SDP version.
    pub const UNSUPPORTED_VERSION: u16 = 0x0001;
    /// The service record handle is invalid.
    pub const INVALID_RECORD_HANDLE: u16 = 0x0002;
    /// The request was malformed.
    pub const INVALID_REQUEST_SYNTAX: u16 = 0x0003;
    /// A PDU size was wrong.
    pub const INVALID_PDU_SIZE: u16 = 0x0004;
    /// The continuation state was not one we issued.
    pub const INVALID_CONTINUATION_STATE: u16 = 0x0005;
    /// Out of resources.
    pub const INSUFFICIENT_RESOURCES: u16 = 0x0006;
}

/// One entry in an attribute id list: a single id or an inclusive range.
///
/// Ranges are how a peer asks for "everything" (`0x0000..=0xFFFF`), which is what most
/// stacks send, so treating the list as single ids only returns an empty record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeRange {
    /// One attribute id.
    Single(u16),
    /// An inclusive range of ids.
    Range(u16, u16),
}

impl AttributeRange {
    /// Whether `id` falls in this entry.
    #[must_use]
    pub const fn contains(self, id: u16) -> bool {
        match self {
            Self::Single(x) => id == x,
            Self::Range(lo, hi) => id >= lo && id <= hi,
        }
    }

    /// Append this entry to an attribute-id-list body.
    ///
    /// The width is the whole distinction: a single id is a **16-bit** uint and a range
    /// is a **32-bit** uint with the bounds packed high/low. That means
    /// `Range(0x0000, 0xFFFF)` — the "give me everything" every stack sends — packs to
    /// the value `0x0000FFFF`, which is numerically identical to `Single(0xFFFF)`. Any
    /// encoder that picks the narrowest width for its integers silently turns the first
    /// into the second, and the peer gets an empty record back.
    fn encode_into(self, body: &mut BytesMut) {
        const UINT_16: u8 = (1 << 3) | 1;
        const UINT_32: u8 = (1 << 3) | 2;
        match self {
            Self::Single(id) => {
                body.put_u8(UINT_16);
                body.put_u16(id);
            }
            Self::Range(lo, hi) => {
                body.put_u8(UINT_32);
                body.put_u16(lo);
                body.put_u16(hi);
            }
        }
    }
}

/// Continuation state: an opaque token the server issues to resume a long response.
///
/// Capped at 16 bytes by the spec. We use two — an offset into the full response — but
/// must accept whatever a peer echoes back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Continuation(pub Bytes);

impl Continuation {
    /// No continuation: the response is complete.
    #[must_use]
    pub fn none() -> Self {
        Self(Bytes::new())
    }

    /// A continuation resuming at `offset` bytes into the full response.
    #[must_use]
    pub fn at(offset: u16) -> Self {
        Self(Bytes::copy_from_slice(&offset.to_be_bytes()))
    }

    /// The offset this token encodes, if it is one of ours.
    #[must_use]
    pub fn offset(&self) -> Option<u16> {
        (self.0.len() == 2).then(|| u16::from_be_bytes([self.0[0], self.0[1]]))
    }

    /// Whether more data follows.
    #[must_use]
    pub fn is_more(&self) -> bool {
        !self.0.is_empty()
    }

    fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(u8::try_from(self.0.len()).unwrap_or(0));
        buf.extend_from_slice(&self.0);
    }

    fn decode(buf: &[u8]) -> Result<(Self, usize), SdpError> {
        let &len = buf.first().ok_or(SdpError::Truncated {
            what: "continuation length",
            need: 1,
            have: 0,
        })?;
        let len = usize::from(len);
        if len > 16 {
            return Err(SdpError::ContinuationTooLong(len));
        }
        if buf.len() < 1 + len {
            return Err(SdpError::Truncated {
                what: "continuation state",
                need: 1 + len,
                have: buf.len(),
            });
        }
        Ok((Self(Bytes::copy_from_slice(&buf[1..1 + len])), 1 + len))
    }
}

/// A request from a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdpRequest {
    /// Find records matching a service search pattern.
    ServiceSearch {
        /// Transaction id.
        tid: u16,
        /// UUIDs a record must contain to match.
        patterns: Vec<Uuid>,
        /// How many handles the peer will accept.
        max_records: u16,
        /// Continuation token.
        cont: Continuation,
    },
    /// Read attributes from one known record.
    ServiceAttribute {
        /// Transaction id.
        tid: u16,
        /// Which record.
        handle: u32,
        /// Response byte ceiling.
        max_bytes: u16,
        /// Which attributes.
        attributes: Vec<AttributeRange>,
        /// Continuation token.
        cont: Continuation,
    },
    /// Search and read attributes in one round trip.
    ServiceSearchAttribute {
        /// Transaction id.
        tid: u16,
        /// UUIDs a record must contain to match.
        patterns: Vec<Uuid>,
        /// Response byte ceiling.
        max_bytes: u16,
        /// Which attributes.
        attributes: Vec<AttributeRange>,
        /// Continuation token.
        cont: Continuation,
    },
}

impl SdpRequest {
    /// The transaction id, which the response must echo.
    #[must_use]
    pub const fn tid(&self) -> u16 {
        match self {
            Self::ServiceSearch { tid, .. }
            | Self::ServiceAttribute { tid, .. }
            | Self::ServiceSearchAttribute { tid, .. } => *tid,
        }
    }

    /// Encode a complete PDU.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut params = BytesMut::with_capacity(32);
        let id = match self {
            Self::ServiceSearch {
                patterns,
                max_records,
                cont,
                ..
            } => {
                DataElement::uuid_seq(patterns.iter().copied()).encode(&mut params);
                params.put_u16(*max_records);
                cont.encode(&mut params);
                pdu_id::SERVICE_SEARCH_REQUEST
            }
            Self::ServiceAttribute {
                handle,
                max_bytes,
                attributes,
                cont,
                ..
            } => {
                params.put_u32(*handle);
                params.put_u16(*max_bytes);
                params.extend_from_slice(&attribute_list(attributes));
                cont.encode(&mut params);
                pdu_id::SERVICE_ATTRIBUTE_REQUEST
            }
            Self::ServiceSearchAttribute {
                patterns,
                max_bytes,
                attributes,
                cont,
                ..
            } => {
                DataElement::uuid_seq(patterns.iter().copied()).encode(&mut params);
                params.put_u16(*max_bytes);
                params.extend_from_slice(&attribute_list(attributes));
                cont.encode(&mut params);
                pdu_id::SERVICE_SEARCH_ATTRIBUTE_REQUEST
            }
        };
        frame(id, self.tid(), &params)
    }

    /// Decode a complete PDU.
    ///
    /// # Errors
    /// [`SdpError::Truncated`] on a short buffer, [`SdpError::UnsupportedPdu`] for a
    /// request type we don't serve.
    pub fn decode(buf: &[u8]) -> Result<Self, SdpError> {
        let (id, tid, params) = unframe(buf)?;
        match id {
            pdu_id::SERVICE_SEARCH_REQUEST => {
                let (pattern, used) = DataElement::decode(params)?;
                let rest = &params[used..];
                let max_records = be_u16(rest, "max record count")?;
                let (cont, _) = Continuation::decode(&rest[2..])?;
                Ok(Self::ServiceSearch {
                    tid,
                    patterns: uuids(&pattern),
                    max_records,
                    cont,
                })
            }
            pdu_id::SERVICE_ATTRIBUTE_REQUEST => {
                if params.len() < 6 {
                    return Err(SdpError::Truncated {
                        what: "service attribute request",
                        need: 6,
                        have: params.len(),
                    });
                }
                let handle = u32::from_be_bytes([params[0], params[1], params[2], params[3]]);
                let max_bytes = u16::from_be_bytes([params[4], params[5]]);
                let (attributes, used) = decode_attribute_list(&params[6..])?;
                let (cont, _) = Continuation::decode(&params[6 + used..])?;
                Ok(Self::ServiceAttribute {
                    tid,
                    handle,
                    max_bytes,
                    attributes,
                    cont,
                })
            }
            pdu_id::SERVICE_SEARCH_ATTRIBUTE_REQUEST => {
                let (pattern, used) = DataElement::decode(params)?;
                let rest = &params[used..];
                let max_bytes = be_u16(rest, "max attribute byte count")?;
                let (attributes, used2) = decode_attribute_list(&rest[2..])?;
                let (cont, _) = Continuation::decode(&rest[2 + used2..])?;
                Ok(Self::ServiceSearchAttribute {
                    tid,
                    patterns: uuids(&pattern),
                    max_bytes,
                    attributes,
                    cont,
                })
            }
            other => Err(SdpError::UnsupportedPdu(other)),
        }
    }
}

/// A response to a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdpResponse {
    /// The request could not be served.
    Error {
        /// Transaction id.
        tid: u16,
        /// Why.
        code: u16,
    },
    /// Matching record handles.
    ServiceSearch {
        /// Transaction id.
        tid: u16,
        /// Total matches across all continuations.
        total: u16,
        /// Handles in this response.
        handles: Vec<u32>,
        /// Continuation token.
        cont: Continuation,
    },
    /// Attributes of one record, already encoded.
    ServiceAttribute {
        /// Transaction id.
        tid: u16,
        /// Encoded attribute list fragment.
        attributes: Bytes,
        /// Continuation token.
        cont: Continuation,
    },
    /// Attribute lists for every matching record, already encoded.
    ServiceSearchAttribute {
        /// Transaction id.
        tid: u16,
        /// Encoded attribute-lists fragment.
        lists: Bytes,
        /// Continuation token.
        cont: Continuation,
    },
}

impl SdpResponse {
    /// Encode a complete PDU.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut params = BytesMut::with_capacity(64);
        let (id, tid) = match self {
            Self::Error { tid, code } => {
                params.put_u16(*code);
                (pdu_id::ERROR_RESPONSE, *tid)
            }
            Self::ServiceSearch {
                tid,
                total,
                handles,
                cont,
            } => {
                params.put_u16(*total);
                params.put_u16(u16::try_from(handles.len()).unwrap_or(u16::MAX));
                for h in handles {
                    params.put_u32(*h);
                }
                cont.encode(&mut params);
                (pdu_id::SERVICE_SEARCH_RESPONSE, *tid)
            }
            Self::ServiceAttribute {
                tid,
                attributes,
                cont,
            } => {
                params.put_u16(u16::try_from(attributes.len()).unwrap_or(u16::MAX));
                params.extend_from_slice(attributes);
                cont.encode(&mut params);
                (pdu_id::SERVICE_ATTRIBUTE_RESPONSE, *tid)
            }
            Self::ServiceSearchAttribute { tid, lists, cont } => {
                params.put_u16(u16::try_from(lists.len()).unwrap_or(u16::MAX));
                params.extend_from_slice(lists);
                cont.encode(&mut params);
                (pdu_id::SERVICE_SEARCH_ATTRIBUTE_RESPONSE, *tid)
            }
        };
        frame(id, tid, &params)
    }

    /// Decode a complete PDU.
    ///
    /// # Errors
    /// [`SdpError::Truncated`] on a short buffer, [`SdpError::UnsupportedPdu`] for an
    /// unknown response type.
    pub fn decode(buf: &[u8]) -> Result<Self, SdpError> {
        let (id, tid, params) = unframe(buf)?;
        match id {
            pdu_id::ERROR_RESPONSE => Ok(Self::Error {
                tid,
                code: be_u16(params, "error code")?,
            }),
            pdu_id::SERVICE_SEARCH_RESPONSE => {
                let total = be_u16(params, "total record count")?;
                let current = be_u16(&params[2..], "current record count")?;
                let n = usize::from(current);
                if params.len() < 4 + n * 4 {
                    return Err(SdpError::Truncated {
                        what: "service search handles",
                        need: 4 + n * 4,
                        have: params.len(),
                    });
                }
                let handles = (0..n)
                    .map(|i| {
                        let o = 4 + i * 4;
                        u32::from_be_bytes([params[o], params[o + 1], params[o + 2], params[o + 3]])
                    })
                    .collect();
                let (cont, _) = Continuation::decode(&params[4 + n * 4..])?;
                Ok(Self::ServiceSearch {
                    tid,
                    total,
                    handles,
                    cont,
                })
            }
            pdu_id::SERVICE_ATTRIBUTE_RESPONSE | pdu_id::SERVICE_SEARCH_ATTRIBUTE_RESPONSE => {
                let count = usize::from(be_u16(params, "attribute byte count")?);
                if params.len() < 2 + count {
                    return Err(SdpError::Truncated {
                        what: "attribute list",
                        need: 2 + count,
                        have: params.len(),
                    });
                }
                let body = Bytes::copy_from_slice(&params[2..2 + count]);
                let (cont, _) = Continuation::decode(&params[2 + count..])?;
                Ok(if id == pdu_id::SERVICE_ATTRIBUTE_RESPONSE {
                    Self::ServiceAttribute {
                        tid,
                        attributes: body,
                        cont,
                    }
                } else {
                    Self::ServiceSearchAttribute {
                        tid,
                        lists: body,
                        cont,
                    }
                })
            }
            other => Err(SdpError::UnsupportedPdu(other)),
        }
    }
}

fn frame(id: u8, tid: u16, params: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(5 + params.len());
    buf.put_u8(id);
    buf.put_u16(tid);
    buf.put_u16(u16::try_from(params.len()).unwrap_or(u16::MAX));
    buf.extend_from_slice(params);
    buf.freeze()
}

fn unframe(buf: &[u8]) -> Result<(u8, u16, &[u8]), SdpError> {
    if buf.len() < 5 {
        return Err(SdpError::Truncated {
            what: "sdp pdu header",
            need: 5,
            have: buf.len(),
        });
    }
    let id = buf[0];
    let tid = u16::from_be_bytes([buf[1], buf[2]]);
    let len = usize::from(u16::from_be_bytes([buf[3], buf[4]]));
    if buf.len() < 5 + len {
        return Err(SdpError::Truncated {
            what: "sdp pdu body",
            need: 5 + len,
            have: buf.len(),
        });
    }
    Ok((id, tid, &buf[5..5 + len]))
}

fn be_u16(buf: &[u8], what: &'static str) -> Result<u16, SdpError> {
    if buf.len() < 2 {
        return Err(SdpError::Truncated {
            what,
            need: 2,
            have: buf.len(),
        });
    }
    Ok(u16::from_be_bytes([buf[0], buf[1]]))
}

/// Encode an attribute id list as a sequence, preserving each entry's integer width.
///
/// Built from raw bytes rather than via [`DataElement`] because the decoded form cannot
/// carry the width, and the width is what separates a range from a single id.
fn attribute_list(ranges: &[AttributeRange]) -> Bytes {
    const SEQ_U8_LEN: u8 = (6 << 3) | 5;
    const SEQ_U16_LEN: u8 = (6 << 3) | 6;
    let mut body = BytesMut::with_capacity(ranges.len() * 5);
    for r in ranges {
        r.encode_into(&mut body);
    }
    let mut out = BytesMut::with_capacity(body.len() + 3);
    if let Ok(len) = u8::try_from(body.len()) {
        out.put_u8(SEQ_U8_LEN);
        out.put_u8(len);
    } else {
        out.put_u8(SEQ_U16_LEN);
        out.put_u16(u16::try_from(body.len()).unwrap_or(u16::MAX));
    }
    out.extend_from_slice(&body);
    out.freeze()
}

/// Decode an attribute id list, reading each entry's width to tell ranges from singles.
///
/// Returns the entries and the total bytes consumed.
fn decode_attribute_list(buf: &[u8]) -> Result<(Vec<AttributeRange>, usize), SdpError> {
    let (_, header_len, body_len) = DataElement::split_header(buf)?;
    let total = header_len + body_len;
    if buf.len() < total {
        return Err(SdpError::Truncated {
            what: "attribute id list",
            need: total,
            have: buf.len(),
        });
    }
    let mut rest = &buf[header_len..total];
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (_, hl, bl) = DataElement::split_header(rest)?;
        if rest.len() < hl + bl {
            return Err(SdpError::Truncated {
                what: "attribute id entry",
                need: hl + bl,
                have: rest.len(),
            });
        }
        let body = &rest[hl..hl + bl];
        match bl {
            2 => out.push(AttributeRange::Single(u16::from_be_bytes([
                body[0], body[1],
            ]))),
            4 => out.push(AttributeRange::Range(
                u16::from_be_bytes([body[0], body[1]]),
                u16::from_be_bytes([body[2], body[3]]),
            )),
            // Anything else isn't a legal attribute id entry; skip rather than fail, so
            // one odd entry doesn't cost the peer the whole query.
            _ => {}
        }
        rest = &rest[hl + bl..];
    }
    Ok((out, total))
}

fn uuids(e: &DataElement) -> Vec<Uuid> {
    e.as_sequence()
        .map(|items| items.iter().filter_map(DataElement::as_uuid).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_search_attribute_request_round_trips() {
        // The request every phone sends when it wants to know what we are.
        let req = SdpRequest::ServiceSearchAttribute {
            tid: 0x1234,
            patterns: vec![Uuid::AUDIO_SINK],
            max_bytes: 672,
            attributes: vec![AttributeRange::Range(0x0000, 0xFFFF)],
            cont: Continuation::none(),
        };
        assert_eq!(SdpRequest::decode(&req.encode()).unwrap(), req);
    }

    #[test]
    fn a_range_and_a_single_id_are_told_apart_by_width_not_value() {
        // The bug this exists to prevent: Range(0x0000, 0xFFFF) packs to the value
        // 0x0000FFFF, which is numerically identical to Single(0xFFFF). Only the encoded
        // integer width distinguishes them, so an encoder that narrows its integers
        // turns "give me every attribute" into "give me attribute 0xFFFF" — and the peer
        // gets an empty record with no error anywhere.
        let every = AttributeRange::Range(0x0000, 0xFFFF);
        let (back, used) = decode_attribute_list(&attribute_list(&[every])).unwrap();
        assert_eq!(back, vec![every]);
        assert_eq!(used, 7, "sequence header + one 32-bit uint element");

        let one = AttributeRange::Single(0xFFFF);
        let (back, _) = decode_attribute_list(&attribute_list(&[one])).unwrap();
        assert_eq!(back, vec![one]);
        assert_ne!(attribute_list(&[every]), attribute_list(&[one]));
    }

    #[test]
    fn a_mixed_attribute_list_round_trips() {
        let wanted = vec![
            AttributeRange::Single(0x0000),
            AttributeRange::Range(0x0100, 0x0200),
            AttributeRange::Single(0x0311),
        ];
        let (back, _) = decode_attribute_list(&attribute_list(&wanted)).unwrap();
        assert_eq!(back, wanted);
    }

    #[test]
    fn ranges_match_the_ids_inside_them() {
        assert!(AttributeRange::Range(0x0000, 0xFFFF).contains(0x0311));
        assert!(AttributeRange::Single(0x0004).contains(0x0004));
        assert!(!AttributeRange::Single(0x0004).contains(0x0005));
        assert!(!AttributeRange::Range(0x0100, 0x0200).contains(0x0300));
    }

    #[test]
    fn continuation_tokens_carry_an_offset_and_signal_more() {
        assert!(!Continuation::none().is_more());
        let c = Continuation::at(0x0140);
        assert!(c.is_more());
        assert_eq!(c.offset(), Some(0x0140));
    }

    #[test]
    fn an_overlong_continuation_is_refused() {
        // The spec caps it at 16 bytes; a peer claiming more is either broken or
        // attempting to make us allocate on its say-so.
        let mut buf = vec![17u8];
        buf.extend(std::iter::repeat_n(0u8, 17));
        assert!(matches!(
            Continuation::decode(&buf),
            Err(SdpError::ContinuationTooLong(17))
        ));
    }

    #[test]
    fn responses_round_trip_and_echo_the_transaction_id() {
        let rsp = SdpResponse::ServiceSearchAttribute {
            tid: 0xBEEF,
            lists: Bytes::from_static(&[0x35, 0x03, 0x09, 0x11, 0x0b]),
            cont: Continuation::at(5),
        };
        let back = SdpResponse::decode(&rsp.encode()).unwrap();
        assert_eq!(back, rsp);
    }

    #[test]
    fn an_error_response_round_trips() {
        let rsp = SdpResponse::Error {
            tid: 1,
            code: error_code::INVALID_REQUEST_SYNTAX,
        };
        assert_eq!(SdpResponse::decode(&rsp.encode()).unwrap(), rsp);
    }

    #[test]
    fn a_truncated_pdu_is_refused() {
        assert!(matches!(
            SdpRequest::decode(&[0x06, 0x00]),
            Err(SdpError::Truncated { .. })
        ));
    }
}
