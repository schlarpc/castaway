//! SDP data elements: the self-describing type/size encoding every record is built from.

use bytes::{BufMut, BytesMut};

use crate::error::SdpError;
use crate::uuid::{Uuid, UuidWidth};

/// One SDP data element.
///
/// The encoding is a header byte of `(type << 3) | size_index`, then a length for the
/// variable-size indices, then the body. Sequences nest, which is what makes a protocol
/// descriptor list a list of lists rather than a flat blob.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataElement {
    /// The nil element.
    Nil,
    /// An unsigned integer, in the smallest width that holds it.
    ///
    /// Correct for values whose width is nobody's business — a bitmask, a count. Wrong
    /// wherever the spec *fixes* the width: see [`DataElement::Uint16`].
    Uint(u64),
    /// An unsigned integer pinned to 32 bits, whatever its value.
    ///
    /// Service record handles are `uint32` by spec; a low-numbered handle would otherwise
    /// narrow to one or two bytes and shift everything after it.
    Uint32(u32),
    /// An unsigned integer pinned to 16 bits, whatever its value.
    ///
    /// Some fields have a width the spec fixes and the value cannot narrow. An L2CAP PSM
    /// is the one that bit us: `0x0019` fits in a byte, so the narrowest-width encoder
    /// emitted `uint8 0x19`, and a strict peer reading a protocol descriptor that should
    /// hold a `uint16` finds a malformed record and walks away without a word. BlueZ is
    /// lenient and read it fine, which is exactly what made it survive so long.
    ///
    /// Same family as the attribute-id bug: a value-derived width is a decoding hazard
    /// wherever the reader knows what width to expect.
    Uint16(u16),
    /// A signed integer.
    Int(i64),
    /// A UUID.
    Uuid(Uuid),
    /// A text string.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// An ordered sequence.
    Sequence(Vec<DataElement>),
    /// A set of alternatives.
    Alternative(Vec<DataElement>),
    /// A URL.
    Url(String),
}

const TYPE_NIL: u8 = 0;
const TYPE_UINT: u8 = 1;
const TYPE_INT: u8 = 2;
const TYPE_UUID: u8 = 3;
const TYPE_TEXT: u8 = 4;
const TYPE_BOOL: u8 = 5;
const TYPE_SEQUENCE: u8 = 6;
const TYPE_ALTERNATIVE: u8 = 7;
const TYPE_URL: u8 = 8;

/// How deep nested sequences may go before a decode is refused.
///
/// A real record is nowhere near this: a `ServiceSearchAttributeResponse` carrying an
/// `AdditionalProtocolDescriptorLists` — about the deepest thing a phone sends — nests
/// five or six levels. Sixteen leaves room for something exotic and still bounds the
/// stack the decoder's recursion consumes, which is the whole point: the depth is
/// chosen by the peer's bytes, and past a few thousand frames the process aborts
/// instead of erroring (#142).
pub const MAX_DEPTH: usize = 16;

impl DataElement {
    /// Convenience: a sequence of short UUIDs.
    #[must_use]
    pub fn uuid_seq(uuids: impl IntoIterator<Item = Uuid>) -> Self {
        Self::Sequence(uuids.into_iter().map(Self::Uuid).collect())
    }

    /// Encode into `buf`.
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            Self::Nil => buf.put_u8(TYPE_NIL << 3),
            Self::Uint(v) => Self::put_uint(buf, *v),
            Self::Uint16(v) => {
                // Size index 1 == two bytes, regardless of how small the value is.
                buf.put_u8((TYPE_UINT << 3) | 1);
                buf.put_u16(*v);
            }
            Self::Uint32(v) => {
                // Size index 2 == four bytes.
                buf.put_u8((TYPE_UINT << 3) | 2);
                buf.put_u32(*v);
            }
            Self::Int(v) => Self::put_int(buf, *v),
            Self::Uuid(u) => Self::put_uuid(buf, *u),
            Self::Bool(b) => {
                buf.put_u8(TYPE_BOOL << 3);
                buf.put_u8(u8::from(*b));
            }
            Self::Text(s) => Self::put_string(buf, TYPE_TEXT, s.as_bytes()),
            Self::Url(s) => Self::put_string(buf, TYPE_URL, s.as_bytes()),
            Self::Sequence(items) => Self::put_container(buf, TYPE_SEQUENCE, items),
            Self::Alternative(items) => Self::put_container(buf, TYPE_ALTERNATIVE, items),
        }
    }

    /// Encode to a standalone byte vector.
    #[must_use]
    pub fn to_bytes(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(32);
        self.encode(&mut buf);
        buf
    }

    fn put_uint(buf: &mut BytesMut, v: u64) {
        // Smallest width that holds the value: records get noticeably smaller, and some
        // peers cap the response size we can return in one PDU.
        if let Ok(v) = u8::try_from(v) {
            buf.put_u8(TYPE_UINT << 3);
            buf.put_u8(v);
        } else if let Ok(v) = u16::try_from(v) {
            buf.put_u8((TYPE_UINT << 3) | 1);
            buf.put_u16(v);
        } else if let Ok(v) = u32::try_from(v) {
            buf.put_u8((TYPE_UINT << 3) | 2);
            buf.put_u32(v);
        } else {
            buf.put_u8((TYPE_UINT << 3) | 3);
            buf.put_u64(v);
        }
    }

    fn put_int(buf: &mut BytesMut, v: i64) {
        if let Ok(v) = i8::try_from(v) {
            buf.put_u8(TYPE_INT << 3);
            buf.put_i8(v);
        } else if let Ok(v) = i16::try_from(v) {
            buf.put_u8((TYPE_INT << 3) | 1);
            buf.put_i16(v);
        } else if let Ok(v) = i32::try_from(v) {
            buf.put_u8((TYPE_INT << 3) | 2);
            buf.put_i32(v);
        } else {
            buf.put_u8((TYPE_INT << 3) | 3);
            buf.put_i64(v);
        }
    }

    fn put_uuid(buf: &mut BytesMut, u: Uuid) {
        match u.width() {
            UuidWidth::Short => {
                buf.put_u8((TYPE_UUID << 3) | 1);
                buf.put_u16(u.as_short().unwrap_or(0));
            }
            UuidWidth::Medium => {
                buf.put_u8((TYPE_UUID << 3) | 2);
                let b = u.as_bytes();
                buf.put_slice(&b[0..4]);
            }
            UuidWidth::Long => {
                buf.put_u8((TYPE_UUID << 3) | 4);
                buf.put_slice(u.as_bytes());
            }
        }
    }

    fn put_string(buf: &mut BytesMut, kind: u8, body: &[u8]) {
        if let Ok(len) = u8::try_from(body.len()) {
            buf.put_u8((kind << 3) | 5);
            buf.put_u8(len);
        } else {
            buf.put_u8((kind << 3) | 6);
            buf.put_u16(u16::try_from(body.len()).unwrap_or(u16::MAX));
        }
        buf.put_slice(body);
    }

    fn put_container(buf: &mut BytesMut, kind: u8, items: &[Self]) {
        let mut body = BytesMut::with_capacity(32);
        for item in items {
            item.encode(&mut body);
        }
        if let Ok(len) = u8::try_from(body.len()) {
            buf.put_u8((kind << 3) | 5);
            buf.put_u8(len);
        } else {
            buf.put_u8((kind << 3) | 6);
            buf.put_u16(u16::try_from(body.len()).unwrap_or(u16::MAX));
        }
        buf.put_slice(&body);
    }

    /// Decode one element, returning it and the bytes consumed.
    ///
    /// # Errors
    /// [`SdpError::Truncated`] on a short buffer, [`SdpError::BadElement`] on a type or
    /// size combination that isn't legal, [`SdpError::TooDeep`] if sequences nest past
    /// [`MAX_DEPTH`].
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), SdpError> {
        Self::decode_at(buf, 0)
    }

    fn decode_at(buf: &[u8], depth: usize) -> Result<(Self, usize), SdpError> {
        // Nesting is peer-controlled and the recursion is real, so this is the only thing
        // standing between a ~30 kB request of nested sequence headers and a stack
        // overflow — which aborts the process rather than returning an `Err`, on a path
        // any unauthenticated BR-EDR peer can reach (the SDP server answers a search
        // pattern it has not authenticated, and the cover-art client parses whatever a
        // phone returns).
        if depth > MAX_DEPTH {
            return Err(SdpError::TooDeep { limit: MAX_DEPTH });
        }
        let &header = buf.first().ok_or(SdpError::Truncated {
            what: "data element header",
            need: 1,
            have: 0,
        })?;
        let kind = header >> 3;
        let size_index = header & 0x07;

        // Size indices 0..=4 are fixed widths; 5..=7 carry an explicit length.
        let (body_len, header_len) = match size_index {
            0 => (if kind == TYPE_NIL { 0 } else { 1 }, 1),
            1 => (2, 1),
            2 => (4, 1),
            3 => (8, 1),
            4 => (16, 1),
            5 => (usize::from(*at(buf, 1)?), 2),
            6 => (
                usize::from(u16::from_be_bytes([*at(buf, 1)?, *at(buf, 2)?])),
                3,
            ),
            _ => (
                u32::from_be_bytes([*at(buf, 1)?, *at(buf, 2)?, *at(buf, 3)?, *at(buf, 4)?])
                    as usize,
                5,
            ),
        };
        let total = header_len + body_len;
        if buf.len() < total {
            return Err(SdpError::Truncated {
                what: "data element body",
                need: total,
                have: buf.len(),
            });
        }
        let body = &buf[header_len..total];

        let element = match kind {
            TYPE_NIL => Self::Nil,
            TYPE_UINT => Self::Uint(be_uint(body)),
            TYPE_INT => Self::Int(be_int(body)),
            TYPE_UUID => match body.len() {
                2 => Self::Uuid(Uuid::short(u16::from_be_bytes([body[0], body[1]]))),
                4 => Self::Uuid(Uuid::medium(u32::from_be_bytes([
                    body[0], body[1], body[2], body[3],
                ]))),
                16 => {
                    let mut b = [0u8; 16];
                    b.copy_from_slice(body);
                    Self::Uuid(Uuid::long(b))
                }
                other => {
                    return Err(SdpError::BadElement {
                        what: "uuid width",
                        detail: other,
                    })
                }
            },
            TYPE_TEXT => Self::Text(String::from_utf8_lossy(body).into_owned()),
            TYPE_URL => Self::Url(String::from_utf8_lossy(body).into_owned()),
            TYPE_BOOL => Self::Bool(body.first().is_some_and(|&b| b != 0)),
            TYPE_SEQUENCE | TYPE_ALTERNATIVE => {
                let mut items = Vec::new();
                let mut rest = body;
                while !rest.is_empty() {
                    let (item, used) = Self::decode_at(rest, depth + 1)?;
                    items.push(item);
                    rest = &rest[used..];
                }
                if kind == TYPE_SEQUENCE {
                    Self::Sequence(items)
                } else {
                    Self::Alternative(items)
                }
            }
            other => {
                return Err(SdpError::BadElement {
                    what: "element type",
                    detail: usize::from(other),
                })
            }
        };
        Ok((element, total))
    }

    /// Split an element header, returning `(type, header length, body length)`.
    ///
    /// Exposed because a few SDP structures are width-sensitive in a way the decoded
    /// [`DataElement`] cannot express: an attribute id list distinguishes a single id
    /// from a range *by the integer's encoded width*, not by its value, so
    /// `Range(0x0000, 0xFFFF)` and `Single(0xFFFF)` are the same number and different
    /// elements. Callers that care read the width from here.
    ///
    /// # Errors
    /// [`SdpError::Truncated`] if the header runs past the buffer.
    pub fn split_header(buf: &[u8]) -> Result<(u8, usize, usize), SdpError> {
        let &header = buf.first().ok_or(SdpError::Truncated {
            what: "data element header",
            need: 1,
            have: 0,
        })?;
        let kind = header >> 3;
        let (body_len, header_len) = match header & 0x07 {
            0 => (if kind == TYPE_NIL { 0 } else { 1 }, 1),
            1 => (2, 1),
            2 => (4, 1),
            3 => (8, 1),
            4 => (16, 1),
            5 => (usize::from(*at(buf, 1)?), 2),
            6 => (
                usize::from(u16::from_be_bytes([*at(buf, 1)?, *at(buf, 2)?])),
                3,
            ),
            _ => (
                u32::from_be_bytes([*at(buf, 1)?, *at(buf, 2)?, *at(buf, 3)?, *at(buf, 4)?])
                    as usize,
                5,
            ),
        };
        Ok((kind, header_len, body_len))
    }

    /// The elements of a sequence or alternative, if this is one.
    #[must_use]
    pub fn as_sequence(&self) -> Option<&[Self]> {
        match self {
            Self::Sequence(v) | Self::Alternative(v) => Some(v),
            _ => None,
        }
    }

    /// The UUID, if this element is one.
    #[must_use]
    pub const fn as_uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(u) => Some(*u),
            _ => None,
        }
    }

    /// The unsigned value, if this element is one.
    #[must_use]
    pub const fn as_uint(&self) -> Option<u64> {
        match self {
            Self::Uint(v) => Some(*v),
            Self::Uint16(v) => Some(*v as u64),
            Self::Uint32(v) => Some(*v as u64),
            _ => None,
        }
    }
}

fn at(buf: &[u8], i: usize) -> Result<&u8, SdpError> {
    buf.get(i).ok_or(SdpError::Truncated {
        what: "data element length",
        need: i + 1,
        have: buf.len(),
    })
}

fn be_uint(body: &[u8]) -> u64 {
    body.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

fn be_int(body: &[u8]) -> i64 {
    let raw = be_uint(body);
    let bits = body.len() * 8;
    if bits < 64 && raw & (1 << (bits - 1)) != 0 {
        // Sign-extend: a negative one-byte int read as unsigned becomes 255, and SDP
        // does carry signed values in a few profile attributes. `wrapping_sub` on the
        // unsigned value then reinterpreting is exact for every width below 64 bits and
        // needs no lossy cast to reason about.
        raw.wrapping_sub(1u64 << bits).cast_signed()
    } else {
        raw.cast_signed()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    fn round_trip(e: &DataElement) -> DataElement {
        let bytes = e.to_bytes();
        let (back, used) = DataElement::decode(&bytes).unwrap();
        assert_eq!(
            used,
            bytes.len(),
            "decode must consume exactly what encode wrote"
        );
        back
    }

    #[test]
    fn integers_use_the_narrowest_width_that_fits() {
        // Records get materially smaller, which matters because a response that
        // overflows the MTU has to be split across a continuation round trip.
        assert_eq!(&DataElement::Uint(5).to_bytes()[..], &hex!("08 05"));
        assert_eq!(&DataElement::Uint(0x0103).to_bytes()[..], &hex!("09 01 03"));
        assert_eq!(
            &DataElement::Uint(0x0001_0203).to_bytes()[..],
            &hex!("0a 00 01 02 03")
        );
        for v in [
            0u64,
            1,
            255,
            256,
            65535,
            65536,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            assert_eq!(round_trip(&DataElement::Uint(v)), DataElement::Uint(v));
        }
    }

    #[test]
    fn a_short_uuid_encodes_in_two_bytes() {
        assert_eq!(
            &DataElement::Uuid(Uuid::AUDIO_SINK).to_bytes()[..],
            &hex!("19 11 0b")
        );
        assert_eq!(
            round_trip(&DataElement::Uuid(Uuid::AUDIO_SINK)),
            DataElement::Uuid(Uuid::AUDIO_SINK)
        );
    }

    #[test]
    fn nested_sequences_round_trip() {
        // A protocol descriptor list is a sequence of sequences — the shape that would
        // flatten into nonsense if nesting were mishandled.
        let pdl = DataElement::Sequence(vec![
            DataElement::Sequence(vec![
                DataElement::Uuid(Uuid::L2CAP),
                DataElement::Uint(0x0019),
            ]),
            DataElement::Sequence(vec![
                DataElement::Uuid(Uuid::AVDTP),
                DataElement::Uint(0x0103),
            ]),
        ]);
        let back = round_trip(&pdl);
        assert_eq!(back, pdl);
        let outer = back.as_sequence().unwrap();
        assert_eq!(outer.len(), 2);
        assert_eq!(outer[0].as_sequence().unwrap()[1].as_uint(), Some(0x0019));
    }

    #[test]
    fn long_strings_switch_to_the_two_byte_length_form() {
        let long = "x".repeat(300);
        let e = DataElement::Text(long.clone());
        let bytes = e.to_bytes();
        assert_eq!(bytes[0], (TYPE_TEXT << 3) | 6, "size index 6 = u16 length");
        assert_eq!(round_trip(&e), DataElement::Text(long));
    }

    #[test]
    fn negative_integers_sign_extend() {
        // Read as unsigned, -1 in one byte becomes 255 and the attribute is nonsense.
        assert_eq!(round_trip(&DataElement::Int(-1)), DataElement::Int(-1));
        assert_eq!(
            round_trip(&DataElement::Int(-32768)),
            DataElement::Int(-32768)
        );
        assert_eq!(round_trip(&DataElement::Int(127)), DataElement::Int(127));
    }

    #[test]
    fn nil_carries_no_body() {
        assert_eq!(&DataElement::Nil.to_bytes()[..], &hex!("00"));
        assert_eq!(round_trip(&DataElement::Nil), DataElement::Nil);
    }

    #[test]
    fn a_truncated_element_is_refused() {
        assert!(matches!(
            DataElement::decode(&hex!("09 01")),
            Err(SdpError::Truncated { .. })
        ));
        assert!(matches!(
            DataElement::decode(&[]),
            Err(SdpError::Truncated { .. })
        ));
    }

    #[test]
    fn an_illegal_uuid_width_is_refused() {
        // Size index 3 = 8 bytes, which is not a legal UUID width.
        assert!(matches!(
            DataElement::decode(&hex!("1b 00 00 00 00 00 00 00 00")),
            Err(SdpError::BadElement { .. })
        ));
    }

    /// `depth` nested sequence headers, each declaring the rest of the buffer as its
    /// body — the shape the overflow was measured with (`36 hi lo`, three bytes a level).
    fn nested_sequences(depth: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(depth * 3);
        for i in 0..depth {
            // Type 6 (sequence), size index 6 (two length bytes).
            buf.push((TYPE_SEQUENCE << 3) | 6);
            let remaining = u16::try_from((depth - i - 1) * 3).unwrap();
            buf.extend_from_slice(&remaining.to_be_bytes());
        }
        buf
    }

    #[test]
    fn nesting_deeper_than_the_ceiling_is_an_error_not_a_stack_overflow() {
        // The depth is chosen by the peer's bytes, on paths an unauthenticated BR-EDR
        // peer reaches: the SDP server decodes a search pattern before anything has
        // authenticated, and the cover-art client decodes whatever a phone returns.
        // Unbounded, a few tens of kB of these headers aborts the process — a stack
        // overflow is not a catchable `Err`, so no amount of caller care recovers it.
        assert!(matches!(
            DataElement::decode(&nested_sequences(MAX_DEPTH + 2)),
            Err(SdpError::TooDeep { limit: MAX_DEPTH })
        ));
        // Nothing near the ceiling is refused: real records nest five or six deep.
        assert!(DataElement::decode(&nested_sequences(MAX_DEPTH)).is_ok());
        // …and a payload that would have overflowed the stack now just errors.
        assert!(matches!(
            DataElement::decode(&nested_sequences(10_000)),
            Err(SdpError::TooDeep { .. })
        ));
    }
}
