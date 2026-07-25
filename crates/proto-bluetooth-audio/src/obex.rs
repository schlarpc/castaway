//! OBEX, and the BIP cover-art client that album art actually arrives over.
//!
//! This is the last mile of the artwork path: AVRCP hands us an image *handle*
//! (attribute 8), and this module turns that handle into JPEG bytes over a separate
//! L2CAP channel to the PSM found in the peer's SDP record. `obexd` has no BIP client, so
//! this is the piece that does not exist on any OS stack.
//!
//! We fetch the **linked thumbnail** rather than the full image. `x-bt/img-thumb` returns
//! a fixed 200×200 JPEG with no image descriptor to negotiate, which is both simpler and
//! the right size for a now-playing card; `x-bt/img-img` requires describing the exact
//! encoding and dimensions wanted and is where interop goes to die.

use bytes::{BufMut, Bytes, BytesMut};
use castaway_core::{Artwork, ImageFormat};

use crate::error::AudioError;

/// OBEX opcodes. The high bit marks the final packet of a request.
pub mod op {
    /// Connect.
    pub const CONNECT: u8 = 0x80;
    /// Disconnect.
    pub const DISCONNECT: u8 = 0x81;
    /// Get, non-final.
    pub const GET: u8 = 0x03;
    /// Get, final.
    pub const GET_FINAL: u8 = 0x83;
    /// Abort.
    pub const ABORT: u8 = 0xFF;
}

/// OBEX response codes, already stripped of the final bit.
pub mod rsp {
    /// More to come; send another GET.
    pub const CONTINUE: u8 = 0x10;
    /// Done.
    pub const SUCCESS: u8 = 0x20;
    /// The handle names nothing.
    pub const NOT_FOUND: u8 = 0x44;
}

/// The BIP Cover Art responder target UUID (AVRCP 1.6).
///
/// Sent in the CONNECT's Target header. Without it the responder has no idea which of
/// its services we mean and refuses the connection.
pub const COVER_ART_TARGET: [u8; 16] = [
    0x71, 0x63, 0xDD, 0x54, 0x4A, 0x7E, 0x11, 0xE2, 0xB4, 0x7C, 0x00, 0x50, 0xC2, 0x49, 0x00, 0x48,
];

/// MIME type for the linked-thumbnail form of a cover-art GET.
pub const TYPE_THUMBNAIL: &str = "x-bt/img-thumb";

/// An OBEX header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Header {
    /// Object name — the image handle, for cover art.
    Name(String),
    /// MIME type of the object requested.
    Type(String),
    /// A chunk of the object.
    Body(Bytes),
    /// The final chunk.
    EndOfBody(Bytes),
    /// Service identifier, sent on CONNECT.
    Target(Bytes),
    /// The responder echoing our target back.
    Who(Bytes),
    /// The session identifier the responder assigned.
    ConnectionId(u32),
    /// Total object length.
    Length(u32),
    /// A header we don't model, kept so it round-trips.
    Other {
        /// Header identifier.
        id: u8,
        /// Raw value.
        value: Bytes,
    },
}

mod hi {
    pub const NAME: u8 = 0x01;
    pub const TYPE: u8 = 0x42;
    pub const BODY: u8 = 0x48;
    pub const END_OF_BODY: u8 = 0x49;
    pub const TARGET: u8 = 0x46;
    pub const WHO: u8 = 0x4A;
    pub const CONNECTION_ID: u8 = 0xCB;
    pub const LENGTH: u8 = 0xC3;
}

impl Header {
    /// Encode into `buf`.
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            // OBEX text headers are **UTF-16 big-endian** and null-terminated. Writing
            // UTF-8 produces a handle the responder cannot match, and the request comes
            // back "not found" as though the art did not exist.
            Self::Name(s) => put_unicode(buf, hi::NAME, s),
            // Type, by contrast, is a *byte sequence* of null-terminated ASCII. The two
            // string headers use different encodings, which is easy to miss.
            Self::Type(s) => {
                let mut v = s.as_bytes().to_vec();
                v.push(0);
                put_bytes(buf, hi::TYPE, &v);
            }
            Self::Body(b) => put_bytes(buf, hi::BODY, b),
            Self::EndOfBody(b) => put_bytes(buf, hi::END_OF_BODY, b),
            Self::Target(b) => put_bytes(buf, hi::TARGET, b),
            Self::Who(b) => put_bytes(buf, hi::WHO, b),
            Self::ConnectionId(v) => {
                buf.put_u8(hi::CONNECTION_ID);
                buf.put_u32(*v);
            }
            Self::Length(v) => {
                buf.put_u8(hi::LENGTH);
                buf.put_u32(*v);
            }
            Self::Other { id, value } => match id >> 6 {
                0b00 | 0b01 => put_bytes(buf, *id, value),
                0b10 => {
                    buf.put_u8(*id);
                    buf.put_u8(value.first().copied().unwrap_or(0));
                }
                _ => {
                    buf.put_u8(*id);
                    let mut four = [0u8; 4];
                    for (slot, byte) in four.iter_mut().zip(value.iter()) {
                        *slot = *byte;
                    }
                    buf.put_slice(&four);
                }
            },
        }
    }

    /// Decode every header in `buf`.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if a header's declared length runs past the buffer.
    pub fn decode_all(mut buf: &[u8]) -> Result<Vec<Self>, AudioError> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            let id = buf[0];
            // The top two bits of the header id encode how wide the value is. Guessing
            // instead of reading them desynchronises the whole header list.
            let (value, used): (Bytes, usize) = match id >> 6 {
                0b00 | 0b01 => {
                    if buf.len() < 3 {
                        return Err(AudioError::Truncated {
                            what: "obex header length",
                            need: 3,
                            have: buf.len(),
                        });
                    }
                    let len = usize::from(u16::from_be_bytes([buf[1], buf[2]]));
                    if len < 3 || buf.len() < len {
                        return Err(AudioError::Truncated {
                            what: "obex header value",
                            need: len.max(3),
                            have: buf.len(),
                        });
                    }
                    (Bytes::copy_from_slice(&buf[3..len]), len)
                }
                0b10 => {
                    if buf.len() < 2 {
                        return Err(AudioError::Truncated {
                            what: "obex byte header",
                            need: 2,
                            have: buf.len(),
                        });
                    }
                    (Bytes::copy_from_slice(&buf[1..2]), 2)
                }
                _ => {
                    if buf.len() < 5 {
                        return Err(AudioError::Truncated {
                            what: "obex quad header",
                            need: 5,
                            have: buf.len(),
                        });
                    }
                    (Bytes::copy_from_slice(&buf[1..5]), 5)
                }
            };
            out.push(match id {
                hi::NAME => Self::Name(from_unicode(&value)),
                hi::TYPE => Self::Type(
                    String::from_utf8_lossy(value.strip_suffix(&[0][..]).unwrap_or(&value))
                        .into_owned(),
                ),
                hi::BODY => Self::Body(value),
                hi::END_OF_BODY => Self::EndOfBody(value),
                hi::TARGET => Self::Target(value),
                hi::WHO => Self::Who(value),
                hi::CONNECTION_ID => Self::ConnectionId(quad(&value)),
                hi::LENGTH => Self::Length(quad(&value)),
                other => Self::Other { id: other, value },
            });
            buf = &buf[used..];
        }
        Ok(out)
    }
}

/// One OBEX packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObexPacket {
    /// Opcode on a request, response code on a response.
    pub code: u8,
    /// Fields that precede the headers (CONNECT only).
    pub prefix: Bytes,
    /// The headers.
    pub headers: Vec<Header>,
}

impl ObexPacket {
    /// Encode packet with its two-byte length.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut body = BytesMut::with_capacity(64);
        body.extend_from_slice(&self.prefix);
        for h in &self.headers {
            h.encode(&mut body);
        }
        let mut buf = BytesMut::with_capacity(3 + body.len());
        buf.put_u8(self.code);
        // The length counts the opcode and the length field itself — unlike L2CAP,
        // where it counts neither.
        buf.put_u16(u16::try_from(3 + body.len()).unwrap_or(u16::MAX));
        buf.extend_from_slice(&body);
        buf.freeze()
    }

    /// Decode a packet, given how many bytes precede the headers.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if shorter than its declared length.
    pub fn decode(buf: &[u8], prefix_len: usize) -> Result<Self, AudioError> {
        if buf.len() < 3 {
            return Err(AudioError::Truncated {
                what: "obex header",
                need: 3,
                have: buf.len(),
            });
        }
        let len = usize::from(u16::from_be_bytes([buf[1], buf[2]]));
        if len < 3 || buf.len() < len {
            return Err(AudioError::Truncated {
                what: "obex packet",
                need: len.max(3),
                have: buf.len(),
            });
        }
        let body = &buf[3..len];
        let split = prefix_len.min(body.len());
        Ok(Self {
            code: buf[0],
            prefix: Bytes::copy_from_slice(&body[..split]),
            headers: Header::decode_all(&body[split..])?,
        })
    }

    /// The response code with its final bit cleared.
    #[must_use]
    pub const fn response(&self) -> u8 {
        self.code & 0x7F
    }
}

/// Where a cover-art fetch has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchState {
    /// Waiting for the CONNECT response.
    Connecting,
    /// Fetching body chunks.
    Fetching,
    /// The image is complete.
    Done,
    /// The peer refused, or has no such image.
    Failed,
}

/// Fetches one image by handle. A sans-I/O state machine: the caller writes
/// [`CoverArtClient::next_request`] to the channel and feeds responses back.
#[derive(Debug)]
pub struct CoverArtClient {
    handle: String,
    state: FetchState,
    connection_id: Option<u32>,
    body: BytesMut,
    max_packet: u16,
}

impl CoverArtClient {
    /// Start a fetch for the image named by an AVRCP attribute-8 handle.
    #[must_use]
    pub fn new(handle: impl Into<String>) -> Self {
        Self {
            handle: handle.into(),
            state: FetchState::Connecting,
            connection_id: None,
            body: BytesMut::new(),
            // 0xFFFF would be legal but a responder may honour it literally and exceed
            // the L2CAP MTU we negotiated; the channel MTU is the real ceiling.
            max_packet: 0x0400,
        }
    }

    /// Where the fetch has got to.
    #[must_use]
    pub const fn state(&self) -> FetchState {
        self.state
    }

    /// The next packet to send, or `None` when there is nothing left to do.
    #[must_use]
    pub fn next_request(&self) -> Option<Bytes> {
        match self.state {
            FetchState::Connecting => {
                let mut prefix = BytesMut::with_capacity(4);
                prefix.put_u8(0x10); // OBEX version 1.0
                prefix.put_u8(0x00); // flags
                prefix.put_u16(self.max_packet);
                Some(
                    ObexPacket {
                        code: op::CONNECT,
                        prefix: prefix.freeze(),
                        headers: vec![Header::Target(Bytes::copy_from_slice(&COVER_ART_TARGET))],
                    }
                    .encode(),
                )
            }
            FetchState::Fetching => {
                let mut headers = Vec::with_capacity(3);
                // The connection id must be echoed on every request after CONNECT, and
                // must come first. Responders reject a GET without it.
                if let Some(id) = self.connection_id {
                    headers.push(Header::ConnectionId(id));
                }
                // Name and Type are only sent on the *first* GET of an object; the
                // continuation GETs carry the connection id alone. Repeating them makes
                // some responders restart the transfer, which never terminates.
                if self.body.is_empty() {
                    headers.push(Header::Type(TYPE_THUMBNAIL.to_owned()));
                    headers.push(Header::Name(self.handle.clone()));
                }
                Some(
                    ObexPacket {
                        code: op::GET_FINAL,
                        prefix: Bytes::new(),
                        headers,
                    }
                    .encode(),
                )
            }
            FetchState::Done | FetchState::Failed => None,
        }
    }

    /// Feed a response packet. Returns the artwork once complete.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] on a malformed packet, or
    /// [`AudioError::BadMediaPacket`] if the responder refused.
    pub fn feed(&mut self, packet: &[u8]) -> Result<Option<Artwork>, AudioError> {
        // A CONNECT response carries four bytes before its headers; every other
        // response carries none.
        let prefix_len = usize::from(self.state == FetchState::Connecting) * 4;
        let pkt = ObexPacket::decode(packet, prefix_len)?;

        match self.state {
            FetchState::Connecting => {
                if pkt.response() != rsp::SUCCESS {
                    self.state = FetchState::Failed;
                    return Err(AudioError::BadMediaPacket(
                        "cover art responder refused connect",
                    ));
                }
                for h in &pkt.headers {
                    if let Header::ConnectionId(id) = h {
                        self.connection_id = Some(*id);
                    }
                }
                self.state = FetchState::Fetching;
                Ok(None)
            }
            FetchState::Fetching => {
                for h in &pkt.headers {
                    match h {
                        Header::Body(b) | Header::EndOfBody(b) => self.body.extend_from_slice(b),
                        _ => {}
                    }
                }
                match pkt.response() {
                    rsp::CONTINUE => Ok(None),
                    rsp::SUCCESS => {
                        self.state = FetchState::Done;
                        let data = self.body.split().freeze();
                        if data.is_empty() {
                            return Err(AudioError::BadMediaPacket("cover art response was empty"));
                        }
                        // The responder does not label the encoding on a thumbnail
                        // fetch — the profile fixes it as JPEG — so sniff rather than
                        // assume, and refuse anything we cannot decode.
                        let format = sniff_format(&data).ok_or(AudioError::BadMediaPacket(
                            "cover art is not a format we decode",
                        ))?;
                        Ok(Some(Artwork::new(format, data)))
                    }
                    _ => {
                        self.state = FetchState::Failed;
                        Err(AudioError::BadMediaPacket("cover art fetch was refused"))
                    }
                }
            }
            FetchState::Done | FetchState::Failed => Ok(None),
        }
    }
}

/// Identify an image by its magic bytes.
#[must_use]
pub fn sniff_format(data: &[u8]) -> Option<ImageFormat> {
    match data {
        [0xFF, 0xD8, 0xFF, ..] => Some(ImageFormat::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [b'G', b'I', b'F', b'8', ..] => Some(ImageFormat::Gif),
        [b'B', b'M', ..] => Some(ImageFormat::Bmp),
        _ => None,
    }
}

fn put_bytes(buf: &mut BytesMut, id: u8, value: &[u8]) {
    buf.put_u8(id);
    buf.put_u16(u16::try_from(3 + value.len()).unwrap_or(u16::MAX));
    buf.put_slice(value);
}

fn put_unicode(buf: &mut BytesMut, id: u8, s: &str) {
    let mut value = BytesMut::with_capacity(s.len() * 2 + 2);
    for unit in s.encode_utf16() {
        value.put_u16(unit);
    }
    value.put_u16(0); // null terminator, also two bytes
    put_bytes(buf, id, &value);
}

fn from_unicode(value: &[u8]) -> String {
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn quad(value: &[u8]) -> u32 {
    let mut four = [0u8; 4];
    for (slot, byte) in four.iter_mut().zip(value.iter()) {
        *slot = *byte;
    }
    u32::from_be_bytes(four)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A minimal JPEG: magic bytes plus filler.
    fn jpeg(len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.resize(len, 0x5A);
        v
    }

    /// Build a responder reply.
    fn reply(code: u8, prefix: &[u8], headers: Vec<Header>) -> Bytes {
        ObexPacket {
            code,
            prefix: Bytes::copy_from_slice(prefix),
            headers,
        }
        .encode()
    }

    /// Drive a client against a scripted responder, returning the artwork.
    fn run(handle: &str, replies: Vec<Bytes>) -> Result<Option<Artwork>, AudioError> {
        let mut client = CoverArtClient::new(handle);
        let mut art = None;
        for reply in replies {
            assert!(client.next_request().is_some(), "client stopped early");
            art = client.feed(&reply)?;
        }
        Ok(art)
    }

    #[test]
    fn name_headers_are_utf16_big_endian_and_null_terminated() {
        // The trap: writing the handle as UTF-8 produces a name the responder cannot
        // match, and the fetch fails as "not found" as though there were no art.
        let mut buf = BytesMut::new();
        Header::Name("0000001".into()).encode(&mut buf);
        assert_eq!(buf[0], hi::NAME);
        assert_eq!(&buf[3..7], &[0x00, b'0', 0x00, b'0'], "UTF-16BE");
        assert_eq!(&buf[buf.len() - 2..], &[0x00, 0x00], "null terminator");

        let back = Header::decode_all(&buf).unwrap();
        assert_eq!(back, vec![Header::Name("0000001".into())]);
    }

    #[test]
    fn type_headers_are_ascii_not_utf16() {
        // Name and Type are both "strings" and use different encodings. Sending Type as
        // UTF-16 makes the responder reject the object type.
        let mut buf = BytesMut::new();
        Header::Type(TYPE_THUMBNAIL.into()).encode(&mut buf);
        assert_eq!(&buf[3..3 + TYPE_THUMBNAIL.len()], TYPE_THUMBNAIL.as_bytes());
        assert_eq!(
            Header::decode_all(&buf).unwrap(),
            vec![Header::Type(TYPE_THUMBNAIL.into())]
        );
    }

    #[test]
    fn header_width_comes_from_the_top_two_bits_of_the_id() {
        // Getting this wrong desynchronises the whole header list rather than failing.
        let mut buf = BytesMut::new();
        Header::ConnectionId(0x1234_5678).encode(&mut buf);
        Header::Length(42).encode(&mut buf);
        Header::Body(Bytes::from_static(&[1, 2, 3])).encode(&mut buf);
        assert_eq!(
            Header::decode_all(&buf).unwrap(),
            vec![
                Header::ConnectionId(0x1234_5678),
                Header::Length(42),
                Header::Body(Bytes::from_static(&[1, 2, 3])),
            ]
        );
    }

    #[test]
    fn an_image_arrives_across_several_body_chunks() {
        // Thumbnails routinely exceed one OBEX packet, so the CONTINUE loop is the
        // normal path rather than an edge case.
        let image = jpeg(600);
        let art = run(
            "0000001",
            vec![
                reply(
                    0xA0,
                    &[0x10, 0x00, 0x04, 0x00],
                    vec![Header::ConnectionId(7)],
                ),
                reply(
                    0x90,
                    &[],
                    vec![Header::Body(Bytes::copy_from_slice(&image[..400]))],
                ),
                reply(
                    0xA0,
                    &[],
                    vec![Header::EndOfBody(Bytes::copy_from_slice(&image[400..]))],
                ),
            ],
        )
        .unwrap()
        .expect("artwork");
        assert_eq!(art.format, ImageFormat::Jpeg);
        assert_eq!(&art.data[..], &image[..]);
    }

    #[test]
    fn the_connection_id_is_echoed_on_every_get_but_name_only_on_the_first() {
        // Repeating Name/Type on continuation GETs makes some responders restart the
        // transfer, which never terminates.
        let mut client = CoverArtClient::new("0000001");
        client
            .feed(&reply(
                0xA0,
                &[0x10, 0x00, 0x04, 0x00],
                vec![Header::ConnectionId(7)],
            ))
            .unwrap();

        let first = ObexPacket::decode(&client.next_request().unwrap(), 0).unwrap();
        assert!(first.headers.contains(&Header::ConnectionId(7)));
        assert!(first
            .headers
            .iter()
            .any(|h| matches!(h, Header::Name(n) if n == "0000001")));

        client
            .feed(&reply(
                0x90,
                &[],
                vec![Header::Body(Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0x00]))],
            ))
            .unwrap();
        let second = ObexPacket::decode(&client.next_request().unwrap(), 0).unwrap();
        assert!(second.headers.contains(&Header::ConnectionId(7)));
        assert!(
            !second.headers.iter().any(|h| matches!(h, Header::Name(_))),
            "continuation GETs must not repeat the name"
        );
    }

    #[test]
    fn a_refused_connect_fails_the_fetch_rather_than_hanging() {
        let mut client = CoverArtClient::new("0000001");
        assert!(client.feed(&reply(0xC3, &[0, 0, 0, 0], vec![])).is_err());
        assert_eq!(client.state(), FetchState::Failed);
        assert!(client.next_request().is_none(), "a failed fetch stops");
    }

    #[test]
    fn a_handle_that_names_nothing_fails_cleanly() {
        // Plenty of tracks have no art. That must degrade to a text-only card, not to a
        // stuck fetch.
        let mut client = CoverArtClient::new("deadbeef");
        client
            .feed(&reply(0xA0, &[0x10, 0x00, 0x04, 0x00], vec![]))
            .unwrap();
        assert!(client.feed(&reply(0xC4, &[], vec![])).is_err());
        assert_eq!(client.state(), FetchState::Failed);
    }

    #[test]
    fn an_image_in_a_format_we_cannot_decode_is_refused() {
        // Better a text-only card than a decoder failure three layers down.
        let mut client = CoverArtClient::new("x");
        client
            .feed(&reply(0xA0, &[0x10, 0x00, 0x04, 0x00], vec![]))
            .unwrap();
        let err = client.feed(&reply(
            0xA0,
            &[],
            vec![Header::EndOfBody(Bytes::from_static(b"RIFF....WEBP"))],
        ));
        assert!(err.is_err());
    }

    #[test]
    fn formats_are_sniffed_from_magic_bytes() {
        assert_eq!(sniff_format(&jpeg(16)), Some(ImageFormat::Jpeg));
        assert_eq!(
            sniff_format(&[0x89, b'P', b'N', b'G', 0x0d]),
            Some(ImageFormat::Png)
        );
        assert_eq!(sniff_format(b"GIF89a"), Some(ImageFormat::Gif));
        assert_eq!(sniff_format(b"BM....."), Some(ImageFormat::Bmp));
        assert_eq!(sniff_format(b"not an image"), None);
    }

    #[test]
    fn the_obex_length_counts_its_own_header() {
        // Unlike L2CAP, whose length counts neither the length nor the CID.
        let pkt = ObexPacket {
            code: op::GET_FINAL,
            prefix: Bytes::new(),
            headers: vec![Header::ConnectionId(1)],
        };
        let bytes = pkt.encode();
        assert_eq!(
            u16::from_be_bytes([bytes[1], bytes[2]]) as usize,
            bytes.len()
        );
    }
}
