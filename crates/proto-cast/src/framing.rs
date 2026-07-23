//! CASTv2 framing: each [`CastMessage`] is sent as a 4-byte big-endian length prefix
//! followed by the protobuf bytes. Pure — the actor feeds it socket reads.

use prost::Message;

use crate::error::CastError;
use crate::proto::CastMessage;

/// Encode a message with its 4-byte big-endian length prefix.
///
/// # Errors
/// [`CastError::Encode`] if protobuf encoding fails (shouldn't for valid messages).
pub fn encode(msg: &CastMessage) -> Result<Vec<u8>, CastError> {
    let mut body = Vec::with_capacity(msg.encoded_len() + 4);
    body.extend_from_slice(&[0, 0, 0, 0]); // placeholder for length
    msg.encode(&mut body)
        .map_err(|e| CastError::Encode(e.to_string()))?;
    let len =
        u32::try_from(body.len() - 4).map_err(|_| CastError::Encode("frame too large".into()))?;
    body[..4].copy_from_slice(&len.to_be_bytes());
    Ok(body)
}

/// Try to decode one frame from the front of `buf`.
///
/// Returns `Ok(None)` if `buf` doesn't yet hold a complete frame (caller reads more).
/// On success returns the message and the number of bytes consumed, so the caller can
/// drain them from its read buffer.
///
/// # Errors
/// [`CastError::Decode`] if a complete frame's protobuf body is malformed.
pub fn try_decode(buf: &[u8]) -> Result<Option<(CastMessage, usize)>, CastError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let total = 4 + len;
    if buf.len() < total {
        return Ok(None);
    }
    let msg = CastMessage::decode(&buf[4..total]).map_err(|e| CastError::Decode(e.to_string()))?;
    Ok(Some((msg, total)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn encode_then_decode() {
        let msg = CastMessage::json("a", "b", "urn:x-cast:c", "{\"k\":1}".into());
        let bytes = encode(&msg).unwrap();
        // 4-byte prefix present and correct.
        let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(declared, bytes.len() - 4);
        let (back, consumed) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(back, msg);
    }

    #[test]
    fn partial_frame_returns_none() {
        let msg = CastMessage::json("a", "b", "urn:x-cast:c", "{}".into());
        let bytes = encode(&msg).unwrap();
        assert!(try_decode(&bytes[..2]).unwrap().is_none()); // header incomplete
        assert!(try_decode(&bytes[..bytes.len() - 1]).unwrap().is_none()); // body short
    }

    #[test]
    fn two_frames_consumed_individually() {
        let m1 = CastMessage::json("a", "b", "urn:x-cast:c", "{}".into());
        let m2 = CastMessage::json("b", "a", "urn:x-cast:d", "[]".into());
        let mut stream = encode(&m1).unwrap();
        stream.extend(encode(&m2).unwrap());
        let (d1, n1) = try_decode(&stream).unwrap().unwrap();
        assert_eq!(d1, m1);
        let (d2, _n2) = try_decode(&stream[n1..]).unwrap().unwrap();
        assert_eq!(d2, m2);
    }
}
