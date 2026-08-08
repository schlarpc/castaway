//! The RTSP connection pump, sans I/O.
//!
//! Both RTSP consumers own a socket loop with the same job: accumulate reads, refuse to
//! buffer toward OOM, drain as many whole messages as arrived, and put each outbound
//! message through the byte transform on its way to the wire. That pump was promised as
//! substrate (architecture §1a's connection actor) and then hand-rolled twice instead —
//! with the copies already disagreeing on the cap by the time #220 caught it. What is
//! shared is exactly the byte discipline; per ground rule 3 it lives here as a pure
//! state machine, and each protocol keeps a thin socket loop that feeds it.
//!
//! [`RtspFramer`] owns the accumulation buffer, the message cap, and the
//! [`ByteTransform`] slot — identity for Miracast, ChaCha20 for AirPlay 2 once
//! pair-verify lands. The socket, the dispatch, and the state machine stay in the
//! `proto-*` crate.

use crate::{parse, write, ByteTransform, Identity, RtspError, RtspMessage};

/// The sans-I/O half of an RTSP connection: bytes in via [`RtspFramer::ingest`],
/// messages out via [`RtspFramer::next_message`], and outbound messages turned into
/// wire bytes by [`RtspFramer::seal`].
pub struct RtspFramer {
    buf: Vec<u8>,
    max_message: usize,
    transform: Box<dyn ByteTransform>,
}

impl std::fmt::Debug for RtspFramer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtspFramer")
            .field("buffered", &self.buf.len())
            .field("max_message", &self.max_message)
            .finish_non_exhaustive()
    }
}

impl RtspFramer {
    /// A framer with the identity transform (Miracast, and AirPlay before pairing).
    ///
    /// `max_message` caps the bytes buffered while waiting for a message to complete: a
    /// message that claims more than the cap is never going to arrive, and the
    /// connection should be dropped rather than buffered toward OOM.
    #[must_use]
    pub fn new(max_message: usize) -> Self {
        Self::with_transform(max_message, Box::new(Identity))
    }

    /// A framer with an explicit byte transform.
    #[must_use]
    pub fn with_transform(max_message: usize, transform: Box<dyn ByteTransform>) -> Self {
        Self {
            buf: Vec::with_capacity(4096),
            max_message,
            transform,
        }
    }

    /// Swap the byte transform — the pair-verify seam: AirPlay 2's control channel
    /// becomes ChaCha20-encrypted mid-connection, from the first byte after the
    /// handshake. The caller owns choosing that moment; the framer just stops being
    /// identity from here on.
    pub fn set_transform(&mut self, transform: Box<dyn ByteTransform>) {
        self.transform = transform;
    }

    /// Feed one chunk as read from the socket.
    ///
    /// Decrypts per chunk, not per accumulated buffer: the transform may be a stream
    /// cipher with position, so re-running it over bytes already decrypted would
    /// desynchronize it.
    ///
    /// # Errors
    /// [`RtspError::TooLarge`] once the buffered bytes exceed the cap without framing a
    /// message — the connection is beyond saving and should be dropped. Also any error
    /// the transform reports.
    pub fn ingest(&mut self, chunk: &[u8]) -> Result<(), RtspError> {
        let mut cleartext = chunk.to_vec();
        self.transform.decrypt_inbound(&mut cleartext)?;
        self.buf.extend_from_slice(&cleartext);
        if self.buf.len() > self.max_message {
            return Err(RtspError::TooLarge {
                limit: self.max_message,
            });
        }
        Ok(())
    }

    /// Drain the next complete message off the front of the buffer, or `None` until
    /// more bytes arrive. Call in a loop after each [`RtspFramer::ingest`]: senders
    /// routinely put two messages in one segment and split one across several.
    ///
    /// # Errors
    /// [`RtspError::Malformed`] if the front of the buffer is not an RTSP message.
    pub fn next_message(&mut self) -> Result<Option<RtspMessage>, RtspError> {
        match parse(&self.buf)? {
            Some((msg, consumed)) => {
                self.buf.drain(..consumed);
                Ok(Some(msg))
            }
            None => Ok(None),
        }
    }

    /// Serialize an outbound message and put it through the transform: the bytes to
    /// write to the socket, exactly as the wire should carry them.
    ///
    /// # Errors
    /// [`RtspError::Write`] if serialization fails, or any error the transform reports.
    pub fn seal<B: AsRef<[u8]>>(
        &mut self,
        msg: &rtsp_types::Message<B>,
    ) -> Result<Vec<u8>, RtspError> {
        let mut bytes = write(msg)?;
        self.transform.encrypt_outbound(&mut bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::cseq;

    const OPTIONS: &[u8] = b"OPTIONS rtsp://10.0.0.1:7000/ RTSP/1.0\r\nCSeq: 3\r\n\r\n";

    #[test]
    fn a_message_split_across_ingests_frames_once_whole() {
        let mut framer = RtspFramer::new(1 << 16);
        let (head, tail) = OPTIONS.split_at(10);
        framer.ingest(head).unwrap();
        assert!(framer.next_message().unwrap().is_none());
        framer.ingest(tail).unwrap();
        let msg = framer.next_message().unwrap().unwrap();
        assert_eq!(cseq(&msg), Some(3));
        assert!(framer.next_message().unwrap().is_none());
    }

    #[test]
    fn two_messages_in_one_ingest_drain_in_order() {
        // The reason next_message is called in a loop: sources routinely coalesce
        // messages into one segment (Miracast's M4+M5 being the canonical case).
        let mut stream = OPTIONS.to_vec();
        stream.extend_from_slice(b"GET_PARAMETER rtsp://10.0.0.1/ RTSP/1.0\r\nCSeq: 4\r\n\r\n");
        let mut framer = RtspFramer::new(1 << 16);
        framer.ingest(&stream).unwrap();
        assert_eq!(cseq(&framer.next_message().unwrap().unwrap()), Some(3));
        assert_eq!(cseq(&framer.next_message().unwrap().unwrap()), Some(4));
        assert!(framer.next_message().unwrap().is_none());
    }

    #[test]
    fn bytes_past_the_cap_without_a_message_are_refused() {
        let mut framer = RtspFramer::new(32);
        // A "message" that will never complete: no double-CRLF in sight.
        let result = framer.ingest(&[b'x'; 64]);
        assert!(matches!(result, Err(RtspError::TooLarge { limit: 32 })));
    }

    #[test]
    fn a_drained_message_frees_its_bytes_against_the_cap() {
        // The cap bounds the *unframed* residue, not connection lifetime throughput.
        let mut framer = RtspFramer::new(OPTIONS.len() + 8);
        for _ in 0..4 {
            framer.ingest(OPTIONS).unwrap();
            assert!(framer.next_message().unwrap().is_some());
        }
    }

    /// A stand-in stream transform with position, like the real ChaCha20 one: XOR with
    /// a rolling counter, so applying it twice to the same bytes does *not* round-trip
    /// unless the position discipline is respected.
    struct RollingXor {
        inbound: u8,
        outbound: u8,
    }

    impl ByteTransform for RollingXor {
        fn decrypt_inbound(&mut self, buf: &mut Vec<u8>) -> Result<(), RtspError> {
            for b in buf.iter_mut() {
                *b ^= self.inbound;
                self.inbound = self.inbound.wrapping_add(1);
            }
            Ok(())
        }
        fn encrypt_outbound(&mut self, buf: &mut Vec<u8>) -> Result<(), RtspError> {
            for b in buf.iter_mut() {
                *b ^= self.outbound;
                self.outbound = self.outbound.wrapping_add(1);
            }
            Ok(())
        }
    }

    #[test]
    fn the_transform_is_applied_per_chunk_with_position_kept() {
        // Encrypt the wire form with one rolling keystream, feed it split into uneven
        // chunks, and require the framer to decrypt each chunk exactly once. Decrypting
        // per accumulated buffer instead would re-run early bytes and corrupt them.
        let mut keystream = RollingXor {
            inbound: 7,
            outbound: 7,
        };
        let mut wire = OPTIONS.to_vec();
        keystream.encrypt_outbound(&mut wire).unwrap();

        let mut framer = RtspFramer::with_transform(
            1 << 16,
            Box::new(RollingXor {
                inbound: 7,
                outbound: 7,
            }),
        );
        let (head, tail) = wire.split_at(13);
        framer.ingest(head).unwrap();
        assert!(framer.next_message().unwrap().is_none());
        framer.ingest(tail).unwrap();
        let msg = framer.next_message().unwrap().unwrap();
        assert_eq!(cseq(&msg), Some(3));
    }

    #[test]
    fn seal_produces_transformed_wire_bytes() {
        use rtsp_types::{Message, Response, StatusCode, Version};
        let resp = Response::builder(Version::V1_0, StatusCode::Ok)
            .header(rtsp_types::headers::CSEQ, "3")
            .empty();
        let msg: Message<Vec<u8>> = resp.map_body(|_| Vec::new()).into();

        let mut identity = RtspFramer::new(1 << 16);
        let clear = identity.seal(&msg).unwrap();
        let (back, _) = parse(&clear).unwrap().unwrap();
        assert_eq!(cseq(&back), Some(3));

        let mut enciphered = RtspFramer::with_transform(
            1 << 16,
            Box::new(RollingXor {
                inbound: 7,
                outbound: 7,
            }),
        );
        let sealed = enciphered.seal(&msg).unwrap();
        assert_ne!(sealed, clear, "the outbound transform must reach the wire");
    }
}
