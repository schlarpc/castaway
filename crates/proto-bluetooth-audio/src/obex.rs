//! OBEX, and the BIP cover-art client that album art actually arrives over.
//!
//! This is the last mile of the artwork path: AVRCP hands us an image *handle*
//! (attribute 8), and this module turns that handle into JPEG bytes over a separate
//! L2CAP channel to the PSM found in the peer's SDP record. `obexd` has no BIP client, so
//! this is the piece that does not exist on any OS stack.
//!
//! We fetch the **linked thumbnail** rather than the full image. `x-bt/img-thm` returns a
//! fixed 200×200 JPEG with no image descriptor to negotiate, which is both simpler and
//! the right size for a now-playing card; `x-bt/img-img` requires describing the exact
//! encoding and dimensions wanted and is where interop goes to die.
//!
//! The session is opened **once per link and held**, not once per image. That is not an
//! optimisation: a Target strips attribute 8 from its metadata response when no BIP
//! client is connected, so a receiver that only connects after seeing a handle never sees
//! one (Q29). Connecting first is what makes the handle appear.

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
///
/// `x-bt/img-thm`, not `x-bt/img-thumb`: BIP spells it abbreviated, and a responder that
/// does not recognise the type answers "bad request" rather than "no such image", which
/// reads as a broken handle rather than a typo three layers up.
pub const TYPE_THUMBNAIL: &str = "x-bt/img-thm";

/// An OBEX header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Header {
    /// Object name. **Not** where a cover-art handle goes — see [`Header::ImageHandle`].
    Name(String),
    /// The BIP image handle, in the application-specific header BIP defines for it.
    ///
    /// The distinction is load-bearing and easy to miss because both are Unicode text
    /// headers with identical encoding: the responder matches an image by `Img-Handle`
    /// and ignores `Name`, so putting the handle in `Name` produces a GET with no handle
    /// at all. BlueZ's BIP client builds exactly this header (`IMG_HANDLE_TAG 0x30`).
    ImageHandle(String),
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
    /// BIP's `Img-Handle`. In the user-defined range, and a length-prefixed Unicode
    /// header like `Name` — same encoding, different identifier, different meaning.
    pub const IMAGE_HANDLE: u8 = 0x30;
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
            Self::ImageHandle(s) => put_unicode(buf, hi::IMAGE_HANDLE, s),
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
                hi::IMAGE_HANDLE => Self::ImageHandle(from_unicode(&value)),
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

/// Where a cover-art session has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchState {
    /// Waiting for the CONNECT response.
    Connecting,
    /// Connected and idle — ready to be asked for an image.
    Ready,
    /// Fetching body chunks.
    Fetching,
    /// The responder refused the session; nothing more will work on this channel.
    Failed,
}

/// One OBEX session to a peer's image server, good for as many images as the link lasts.
///
/// A sans-I/O state machine: the caller writes [`CoverArtSession::next_request`] to the
/// channel and feeds responses back with [`CoverArtSession::feed`].
///
/// It is a *session* rather than a fetch because of the ordering AOSP enforces — a Target
/// strips attribute 8 from its metadata response unless a BIP client is already connected
/// — so this has to be up before the handle is asked for, and staying up across tracks is
/// then free (Q29).
#[derive(Debug)]
pub struct CoverArtSession {
    state: FetchState,
    /// The handle currently being fetched. `None` between images.
    handle: Option<String>,
    connection_id: Option<u32>,
    body: BytesMut,
    max_packet: u16,
}

impl CoverArtSession {
    /// Open a session, receiving packets of at most `max_packet` bytes.
    #[must_use]
    pub fn new(max_packet: u16) -> Self {
        Self {
            state: FetchState::Connecting,
            handle: None,
            connection_id: None,
            body: BytesMut::new(),
            // 0xFFFF would be legal but a responder may honour it literally and exceed
            // the L2CAP MTU we negotiated; the channel MTU is the real ceiling.
            max_packet: max_packet.clamp(0x00FF, 0x1000),
        }
    }

    /// Where the session has got to.
    #[must_use]
    pub const fn state(&self) -> FetchState {
        self.state
    }

    /// Whether the session is connected and free to take an image request.
    ///
    /// This is the flag that decides whether asking for attribute 8 is worth doing at
    /// all: a handle we cannot act on is a handle the Target need not have sent.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, FetchState::Ready)
    }

    /// Ask for the image named by an AVRCP attribute-8 handle.
    ///
    /// Returns `false` if the session is not ready — still connecting, already fetching,
    /// or dead. Refusing rather than queueing is deliberate: a skipped-through album
    /// would otherwise build a backlog of images for tracks nobody is on any more.
    pub fn fetch(&mut self, handle: impl Into<String>) -> bool {
        if !self.is_ready() {
            return false;
        }
        self.handle = Some(handle.into());
        self.body.clear();
        self.state = FetchState::Fetching;
        true
    }

    /// The next packet to send, or `None` when there is nothing to say.
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
                // Type and handle are only sent on the *first* GET of an object; the
                // continuation GETs carry the connection id alone. Repeating them makes
                // some responders restart the transfer, which never terminates.
                if self.body.is_empty() {
                    headers.push(Header::Type(TYPE_THUMBNAIL.to_owned()));
                    if let Some(handle) = &self.handle {
                        headers.push(Header::ImageHandle(handle.clone()));
                    }
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
            FetchState::Ready | FetchState::Failed => None,
        }
    }

    /// Feed a response packet. Returns the artwork once an image is complete.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] on a malformed packet, or
    /// [`AudioError::BadMediaPacket`] if the responder refused. A refused *image* leaves
    /// the session usable — plenty of tracks have no art, and the next one may.
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
                self.state = FetchState::Ready;
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
                        self.state = FetchState::Ready;
                        self.handle = None;
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
                        // One image the responder would not give us. The session lives:
                        // the next track gets its own chance rather than inheriting this
                        // one's bad luck.
                        self.state = FetchState::Ready;
                        self.handle = None;
                        self.body.clear();
                        Err(AudioError::BadMediaPacket("cover art fetch was refused"))
                    }
                }
            }
            FetchState::Ready | FetchState::Failed => Ok(None),
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

    /// A session that has completed its CONNECT and is waiting to be asked for an image.
    fn connected() -> CoverArtSession {
        let mut session = CoverArtSession::new(0x0400);
        session
            .feed(&reply(
                0xA0,
                &[0x10, 0x00, 0x04, 0x00],
                vec![Header::ConnectionId(7)],
            ))
            .unwrap();
        assert!(session.is_ready());
        session
    }

    /// Drive a session against a scripted responder, returning the artwork.
    fn run(handle: &str, replies: Vec<Bytes>) -> Result<Option<Artwork>, AudioError> {
        let mut session = connected();
        assert!(session.fetch(handle));
        let mut art = None;
        for reply in replies {
            assert!(session.next_request().is_some(), "session stopped early");
            art = session.feed(&reply)?;
        }
        Ok(art)
    }

    #[test]
    fn unicode_headers_are_utf16_big_endian_and_null_terminated() {
        // The trap: writing the handle as UTF-8 produces a value the responder cannot
        // match, and the fetch fails as "not found" as though there were no art.
        let mut buf = BytesMut::new();
        Header::ImageHandle("0000001".into()).encode(&mut buf);
        assert_eq!(buf[0], hi::IMAGE_HANDLE);
        assert_eq!(&buf[3..7], &[0x00, b'0', 0x00, b'0'], "UTF-16BE");
        assert_eq!(&buf[buf.len() - 2..], &[0x00, 0x00], "null terminator");

        let back = Header::decode_all(&buf).unwrap();
        assert_eq!(back, vec![Header::ImageHandle("0000001".into())]);
    }

    #[test]
    fn the_image_handle_goes_in_its_own_header_not_in_name() {
        // Both are length-prefixed UTF-16 headers, so the mistake encodes perfectly and
        // reads back perfectly — and the responder, which matches on Img-Handle and
        // ignores Name, answers a GET that named no image at all.
        let mut session = connected();
        assert!(session.fetch("0000001"));
        let get = ObexPacket::decode(&session.next_request().unwrap(), 0).unwrap();
        assert!(
            get.headers.contains(&Header::ImageHandle("0000001".into())),
            "the handle belongs in Img-Handle (0x30): {:?}",
            get.headers
        );
        assert!(
            !get.headers.iter().any(|h| matches!(h, Header::Name(_))),
            "and not in Name, which a BIP responder does not look at"
        );
        assert!(
            get.headers.contains(&Header::Type("x-bt/img-thm".into())),
            "BIP spells the thumbnail type abbreviated"
        );
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
    fn the_connection_id_is_echoed_on_every_get_but_the_handle_only_on_the_first() {
        // Repeating the handle/Type on continuation GETs makes some responders restart
        // the transfer, which never terminates.
        let mut client = connected();
        assert!(client.fetch("0000001"));

        let first = ObexPacket::decode(&client.next_request().unwrap(), 0).unwrap();
        assert!(first.headers.contains(&Header::ConnectionId(7)));
        assert!(first
            .headers
            .iter()
            .any(|h| matches!(h, Header::ImageHandle(n) if n == "0000001")));

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
            !second
                .headers
                .iter()
                .any(|h| matches!(h, Header::ImageHandle(_))),
            "continuation GETs must not repeat the handle"
        );
    }

    #[test]
    fn a_refused_connect_fails_the_session_rather_than_hanging() {
        let mut session = CoverArtSession::new(0x0400);
        assert!(session.feed(&reply(0xC3, &[0, 0, 0, 0], vec![])).is_err());
        assert_eq!(session.state(), FetchState::Failed);
        assert!(session.next_request().is_none(), "a dead session stops");
        assert!(!session.fetch("0000001"), "and takes no more requests");
    }

    #[test]
    fn a_handle_that_names_nothing_leaves_the_session_usable() {
        // Plenty of tracks have no art. That must degrade to a text-only card for *that*
        // track — tearing the session down would cost every track after it as well, and
        // rebuilding it takes an SDP query and a channel.
        let mut session = connected();
        assert!(session.fetch("deadbeef"));
        assert!(session.feed(&reply(0xC4, &[], vec![])).is_err());
        assert!(session.is_ready(), "the next track deserves its own chance");
        assert!(session.fetch("0000002"));
    }

    #[test]
    fn a_second_image_reuses_the_session_rather_than_reconnecting() {
        // The reason this is a session at all: reconnecting per track would put an OBEX
        // CONNECT in front of every image, and — worse — a Target strips attribute 8
        // whenever no BIP client is connected, so the gap between tracks would be a
        // window in which handles stop arriving.
        let mut session = connected();
        for handle in ["0000001", "0000002"] {
            assert!(session.fetch(handle));
            let art = session
                .feed(&reply(
                    0xA0,
                    &[],
                    vec![Header::EndOfBody(Bytes::from(jpeg(64)))],
                ))
                .unwrap()
                .expect("artwork");
            assert_eq!(art.format, ImageFormat::Jpeg);
            assert!(session.is_ready());
        }
    }

    #[test]
    fn a_fetch_while_one_is_already_running_is_refused_not_queued() {
        // A skipped-through album would otherwise build a backlog of images for tracks
        // nobody is on any more.
        let mut session = connected();
        assert!(session.fetch("0000001"));
        assert!(!session.fetch("0000002"));
    }

    #[test]
    fn an_image_in_a_format_we_cannot_decode_is_refused() {
        // Better a text-only card than a decoder failure three layers down.
        let mut session = connected();
        assert!(session.fetch("x"));
        let err = session.feed(&reply(
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
