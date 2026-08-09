//! OBEX, and the BIP cover-art client that album art actually arrives over.
//!
//! This is the last mile of the artwork path: AVRCP hands us an image *handle*
//! (attribute 8), and this module turns that handle into JPEG bytes over a separate
//! L2CAP channel to the PSM found in the peer's SDP record.
//!
//! **This module used to claim no OS stack has a BIP client. That was false**, and the
//! correction is worth keeping because the false version was load-bearing in #74 and in
//! architecture-substrate.md §11.1. BlueZ has had the whole chain since the Collabora
//! cover-art series of August 2024: `bluetoothd` surfaces attribute 8 as the `ImgHandle`
//! key on `org.bluez.MediaPlayer1`, and `obexd` carries a real cover-art client — the
//! `BIP-AVRCP` driver, `org.bluez.obex.Image1`, with `Properties`, `Get` and
//! `GetThumbnail`. Verified against bluez 5.86, which is also the oracle the parsers in
//! here are written against.
//!
//! What is true, and is the actual reason to own this: it is split across two daemons
//! with no in-tree wiring between them (`tools/mpris-proxy.c` is the reference glue),
//! both halves are `[experimental]`, and `Image1` delivers the image as **a file on
//! disk** via a `Transfer1` object rather than as bytes in the process that wants to draw
//! them. None of which we could use regardless: the deploy target is one Windows binary
//! with no `bluetoothd` and no `obexd` anywhere in it.
//!
//! We fetch the **linked thumbnail** rather than the full image. `x-bt/img-thm` returns a
//! fixed 200×200 JPEG with no image descriptor to negotiate, which is both simpler and
//! the right size for a now-playing card; `x-bt/img-img` requires describing the exact
//! encoding and dimensions wanted and is where interop goes to die.
//!
//! The session is opened **once per link and held**, not once per image. That is not an
//! optimisation: a Target strips attribute 8 from its metadata response when no BIP
//! client is connected, so a receiver that only connects after seeing a handle never sees
//! one (#74). Connecting first is what makes the handle appear.

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

/// MIME type for the properties form of a cover-art GET.
///
/// Answers "what forms of this image do you hold?" with an XML listing rather than an
/// image. It needs no image descriptor, so unlike `x-bt/img-img` it can be asked for
/// safely by a client that advertises only the thumbnail — which is what makes it the
/// cheap measurement #75 turns on: whether an iPhone lists anything larger than the fixed
/// 200×200 thumbnail is not a thing this project has ever been able to see.
pub const TYPE_IMAGE_PROPERTIES: &str = "x-bt/img-properties";

/// MIME type for a described-image cover-art GET.
///
/// Unlike the thumbnail, this one names *which* form it wants, in an
/// [`Header::ImageDescription`] alongside it — and the form has to be one the responder
/// listed in its properties document, so it is only askable after
/// [`TYPE_IMAGE_PROPERTIES`] has been read (#75).
pub const TYPE_IMAGE: &str = "x-bt/img-img";

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
    /// BIP's `Img-Description`: the XML naming which form of an image is wanted.
    ///
    /// A **byte sequence**, unlike [`Header::ImageHandle`] beside it, which is UTF-16.
    /// Two application-specific headers on the same request with different encodings is
    /// exactly the trap that cost the handle its first implementation, so the two are
    /// separate variants rather than one with a flag (`obexd/client/bip.c`:
    /// `g_obex_header_new_bytes(IMG_DESC_TAG, ...)` against
    /// `g_obex_header_new_unicode(IMG_HANDLE_TAG, ...)`).
    ImageDescription(String),
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
    /// GOEP 2.0 Single Response Mode. `0x01` enables it: the responder then streams
    /// the whole object unprompted instead of waiting for a GET per chunk.
    ///
    /// Not optional dressing — over L2CAP (which AVRCP cover art always is, GOEP 2.0
    /// §4.6) SRM support is mandatory, and a responder that grants it treats a
    /// continuation GET arriving mid-stream as a protocol violation.
    SingleResponseMode(u8),
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
    /// BIP's `Img-Description`, also user-defined but a *byte sequence*.
    pub const IMAGE_DESCRIPTION: u8 = 0x71;
    pub const TYPE: u8 = 0x42;
    pub const BODY: u8 = 0x48;
    pub const END_OF_BODY: u8 = 0x49;
    pub const TARGET: u8 = 0x46;
    pub const WHO: u8 = 0x4A;
    pub const CONNECTION_ID: u8 = 0xCB;
    pub const LENGTH: u8 = 0xC3;
    /// GOEP 2.0's Single Response Mode, a one-byte header (top bits `0b10`).
    pub const SRM: u8 = 0x97;
}

/// The value that enables SRM in [`Header::SingleResponseMode`].
pub const SRM_ENABLE: u8 = 0x01;

impl Header {
    /// Encode into `buf`.
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            // OBEX text headers are **UTF-16 big-endian** and null-terminated. Writing
            // UTF-8 produces a handle the responder cannot match, and the request comes
            // back "not found" as though the art did not exist.
            Self::Name(s) => put_unicode(buf, hi::NAME, s),
            Self::ImageHandle(s) => put_unicode(buf, hi::IMAGE_HANDLE, s),
            // Bytes, not UTF-16, and not null-terminated: BlueZ writes the XML raw.
            Self::ImageDescription(s) => put_bytes(buf, hi::IMAGE_DESCRIPTION, s.as_bytes()),
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
            Self::SingleResponseMode(v) => {
                buf.put_u8(hi::SRM);
                buf.put_u8(*v);
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
                hi::IMAGE_DESCRIPTION => {
                    Self::ImageDescription(String::from_utf8_lossy(&value).into_owned())
                }
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
                hi::SRM => Self::SingleResponseMode(value.first().copied().unwrap_or(0)),
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

/// Where Single Response Mode stands for the fetch in flight.
///
/// An enum rather than a bool because the dangerous state is the *unanswered* one: until
/// the responder's first packet arrives we do not know which conversation we are in, and
/// sending a continuation GET on a guess is exactly the protocol violation SRM exists to
/// rule out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Srm {
    /// Offered on the first GET; the responder has not answered yet.
    Offered,
    /// Granted: the responder streams every remaining chunk unprompted, and a GET from
    /// us mid-stream is a violation (observed: an iPhone drops the whole image channel,
    /// and sometimes the ACL link with it).
    Active,
    /// The responder did not echo the offer: classic request/response continuation.
    Declined,
}

/// One OBEX session to a peer's image server, good for as many images as the link lasts.
///
/// A sans-I/O state machine: the caller writes [`CoverArtSession::next_request`] to the
/// channel and feeds responses back with [`CoverArtSession::feed`].
///
/// It is a *session* rather than a fetch because of the ordering AOSP enforces — a Target
/// strips attribute 8 from its metadata response unless a BIP client is already connected
/// — so this has to be up before the handle is asked for, and staying up across tracks is
/// then free (#74).
/// What the session is currently asking the responder for.
///
/// The type header and what to do with the bytes both follow from this, so carrying it
/// as one value rather than a handle plus a flag is what stops a properties document
/// being sniffed for JPEG magic — the shape of bug the whole artwork path is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fetch {
    /// `x-bt/img-thm`: the 200×200 linked thumbnail.
    Thumbnail(String),
    /// `x-bt/img-properties`: the listing of what the peer holds (#75).
    Properties(String),
    /// `x-bt/img-img`: a named form from that listing.
    Image {
        /// The image.
        handle: String,
        /// Which of its listed forms, as the descriptor XML that will be sent.
        descriptor: String,
    },
}

impl Fetch {
    /// The handle being asked about.
    fn handle(&self) -> &str {
        match self {
            Self::Thumbnail(h) | Self::Properties(h) | Self::Image { handle: h, .. } => h,
        }
    }

    /// The MIME type that names this form.
    const fn mime(&self) -> &'static str {
        match self {
            Self::Thumbnail(_) => TYPE_THUMBNAIL,
            Self::Properties(_) => TYPE_IMAGE_PROPERTIES,
            Self::Image { .. } => TYPE_IMAGE,
        }
    }

    /// The image descriptor this fetch carries, if it names a form.
    fn descriptor(&self) -> Option<&str> {
        match self {
            Self::Image { descriptor, .. } => Some(descriptor),
            Self::Thumbnail(_) | Self::Properties(_) => None,
        }
    }
}

/// A completed OBEX object.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fetched {
    /// The thumbnail, in a format we know how to decode.
    Artwork(Artwork),
    /// What forms of the image the peer holds.
    Properties(ImageProperties),
}

#[derive(Debug)]
pub struct CoverArtSession {
    state: FetchState,
    /// What is currently being fetched. `None` between objects.
    fetch: Option<Fetch>,
    connection_id: Option<u32>,
    body: BytesMut,
    max_packet: u16,
    /// SRM for the fetch in flight. `None` until the first GET of an object has been
    /// produced — which is also what makes that GET recognisably the first, carrying
    /// the type, the handle, and the SRM offer.
    srm: Option<Srm>,
}

impl CoverArtSession {
    /// Open a session, receiving packets of at most `max_packet` bytes.
    #[must_use]
    pub fn new(max_packet: u16) -> Self {
        Self {
            state: FetchState::Connecting,
            fetch: None,
            connection_id: None,
            body: BytesMut::new(),
            // 0xFFFF would be legal but a responder may honour it literally and exceed
            // the L2CAP MTU we negotiated; the channel MTU is the real ceiling.
            max_packet: max_packet.clamp(0x00FF, 0x1000),
            srm: None,
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

    /// Ask for the thumbnail named by an AVRCP attribute-8 handle.
    ///
    /// Returns `false` if the session is not ready — still connecting, already fetching,
    /// or dead. Refusing rather than queueing is deliberate: a skipped-through album
    /// would otherwise build a backlog of images for tracks nobody is on any more.
    pub fn fetch_thumbnail(&mut self, handle: impl Into<String>) -> bool {
        self.start(Fetch::Thumbnail(handle.into()))
    }

    /// Ask what forms of that image the peer actually holds.
    ///
    /// Same channel, same session, same refusal rule as [`Self::fetch_thumbnail`] — this
    /// is a GET for a different MIME type, not a different conversation. Safe to ask for
    /// from a client advertising only the linked thumbnail: unlike `x-bt/img-img` it
    /// carries no image descriptor, so there is nothing to get wrong (#75).
    pub fn fetch_properties(&mut self, handle: impl Into<String>) -> bool {
        self.start(Fetch::Properties(handle.into()))
    }

    /// Ask for one form of an image, at a size the peer's own listing allows.
    ///
    /// Takes a [`ChosenImage`] rather than a width and a height because BIP requires the
    /// descriptor to match what the responder advertised: the only way to obtain one is to
    /// pick it out of that peer's [`ImageProperties`], so a form we invented cannot be
    /// requested and a range cannot be asked for verbatim.
    ///
    /// Returns `false` if the session is busy.
    pub fn fetch_image(&mut self, handle: impl Into<String>, image: ChosenImage<'_>) -> bool {
        self.start(Fetch::Image {
            handle: handle.into(),
            descriptor: image.descriptor(),
        })
    }

    /// Begin a fetch, if the session is free to take one.
    fn start(&mut self, fetch: Fetch) -> bool {
        if !self.is_ready() {
            return false;
        }
        self.fetch = Some(fetch);
        self.body.clear();
        self.srm = None;
        self.state = FetchState::Fetching;
        true
    }

    /// The next packet to send, or `None` when there is nothing to say.
    ///
    /// `&mut self` because producing a request *is* a state transition: handing out the
    /// first GET of an object is what marks SRM as offered, and under an active SRM
    /// grant this deliberately answers `None` — the responder is streaming, and the one
    /// thing a client must not do is ask again.
    pub fn next_request(&mut self) -> Option<Bytes> {
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
            FetchState::Fetching => match self.srm {
                // The first GET of the object: type, handle, and the SRM offer.
                // Repeating type/handle on later GETs makes some responders restart
                // the transfer, which never terminates.
                None => {
                    let mut headers = Vec::with_capacity(4);
                    // The connection id must be echoed on every request after CONNECT,
                    // and must come first. Responders reject a GET without it.
                    if let Some(id) = self.connection_id {
                        headers.push(Header::ConnectionId(id));
                    }
                    // Offered on every fetch: AVRCP cover art is always GOEP 2.0 over
                    // L2CAP, where SRM support is mandatory (§4.6), and reference
                    // clients (bluez obexd) always ask. Before this offer existed we
                    // answered an iPhone's streamed chunks with continuation GETs and
                    // it dropped the channel — see `Srm`.
                    headers.push(Header::SingleResponseMode(SRM_ENABLE));
                    if let Some(fetch) = &self.fetch {
                        headers.push(Header::Type(fetch.mime().to_owned()));
                        headers.push(Header::ImageHandle(fetch.handle().to_owned()));
                        if let Some(descriptor) = fetch.descriptor() {
                            headers.push(Header::ImageDescription(descriptor.to_owned()));
                        }
                    }
                    self.srm = Some(Srm::Offered);
                    Some(
                        ObexPacket {
                            code: op::GET_FINAL,
                            prefix: Bytes::new(),
                            headers,
                        }
                        .encode(),
                    )
                }
                // Streaming: the responder sends the rest unprompted. Asking again is
                // the protocol violation this enum exists to make unrepresentable.
                Some(Srm::Active) => None,
                // Waiting on the first response; the GET is already in flight.
                Some(Srm::Offered) => None,
                // Classic request/response: every further chunk is asked for, with the
                // connection id alone.
                Some(Srm::Declined) => {
                    let headers = match self.connection_id {
                        Some(id) => vec![Header::ConnectionId(id)],
                        None => Vec::new(),
                    };
                    Some(
                        ObexPacket {
                            code: op::GET_FINAL,
                            prefix: Bytes::new(),
                            headers,
                        }
                        .encode(),
                    )
                }
            },
            FetchState::Ready | FetchState::Failed => None,
        }
    }

    /// Feed a response packet. Returns the object once one is complete.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] on a malformed packet, or
    /// [`AudioError::BadMediaPacket`] if the responder refused. A refused *object* leaves
    /// the session usable — plenty of tracks have no art, and the next one may.
    pub fn feed(&mut self, packet: &[u8]) -> Result<Option<Fetched>, AudioError> {
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
                // The first response settles which conversation this is: a responder
                // that grants SRM echoes the enable back and then streams; one that
                // ignores it expects a GET per chunk.
                if self.srm == Some(Srm::Offered) {
                    let granted = pkt
                        .headers
                        .iter()
                        .any(|h| matches!(h, Header::SingleResponseMode(SRM_ENABLE)));
                    self.srm = Some(if granted { Srm::Active } else { Srm::Declined });
                }
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
                        let fetch = self.fetch.take();
                        self.srm = None;
                        let data = self.body.split().freeze();
                        if data.is_empty() {
                            return Err(AudioError::BadMediaPacket("cover art response was empty"));
                        }
                        match fetch {
                            // The responder does not label the encoding on a thumbnail
                            // fetch — the profile fixes it as JPEG — so sniff rather than
                            // assume, and refuse anything we cannot decode.
                            Some(Fetch::Thumbnail(_) | Fetch::Image { .. }) | None => {
                                let format =
                                    sniff_format(&data).ok_or(AudioError::BadMediaPacket(
                                        "cover art is not a format we decode",
                                    ))?;
                                Ok(Some(Fetched::Artwork(Artwork::new(format, data))))
                            }
                            Some(Fetch::Properties(_)) => {
                                Ok(Some(Fetched::Properties(ImageProperties::parse(&data)?)))
                            }
                        }
                    }
                    _ => {
                        // One object the responder would not give us. The session lives:
                        // the next track gets its own chance rather than inheriting this
                        // one's bad luck.
                        self.state = FetchState::Ready;
                        self.fetch = None;
                        self.body.clear();
                        self.srm = None;
                        Err(AudioError::BadMediaPacket("cover art fetch was refused"))
                    }
                }
            }
            FetchState::Ready | FetchState::Failed => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Image properties: what the peer holds, as opposed to what it hands over.
// ---------------------------------------------------------------------------

/// An encoding token from a BIP image descriptor.
///
/// Two states rather than an `Option<ImageFormat>` beside a `String`, because the pair
/// has an illegal combination — a known format with a mismatched token — and the whole
/// point of reading a properties listing is to record what the peer said, including the
/// parts we cannot act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoding {
    /// One we can decode, beside the token exactly as the peer spelled it.
    ///
    /// The token is kept even though the format is known, because a `GetImage` descriptor
    /// has to echo the responder's own spelling back at it: BIP compares encodings by
    /// string (`obexd`, `convBIP2IM`), so re-deriving `"JPEG"` from the enum would send a
    /// peer that wrote `"jpeg"` a form it never offered.
    Known(ImageFormat, String),
    /// One we cannot, kept exactly as sent.
    Unknown(String),
}

impl Encoding {
    /// Parse a BIP `encoding` token.
    #[must_use]
    pub fn parse(token: &str) -> Self {
        ImageFormat::parse(token).map_or_else(
            || Self::Unknown(token.to_owned()),
            |format| Self::Known(format, token.to_owned()),
        )
    }

    /// The format, if it is one the pipeline can decode.
    #[must_use]
    pub const fn format(&self) -> Option<ImageFormat> {
        match self {
            Self::Known(format, _) => Some(*format),
            Self::Unknown(_) => None,
        }
    }

    /// The token as the peer wrote it.
    #[must_use]
    pub fn token(&self) -> &str {
        match self {
            Self::Known(_, token) | Self::Unknown(token) => token,
        }
    }
}

/// The `pixel` attribute of a BIP image descriptor.
///
/// BIP defines exactly three spellings, and they mean genuinely different things:
/// `200*200` is one image the peer holds, `80*60-640*480` is a range it will *transcode*
/// into on request, and `80**-640*480` is that range with the aspect ratio pinned, so the
/// lower bound states a width only. Reading the second as the first records a peer that
/// offers 640×480 as one that offers 80×60.
///
/// Three variants for three grammatical forms, so "a fixed-ratio range with a lower-bound
/// height" is unrepresentable rather than a `None` that every reader has to remember to
/// check. The grammar is `obexd/client/bip-common.c::parse_pixel_range`, which is three
/// anchored regexes — this is those regexes, and nothing looser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelSize {
    /// An exact size: `200*200`.
    Fixed {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
    },
    /// A range the responder transcodes within: `80*60-640*480`.
    Range {
        /// Lower-bound width.
        min_width: u16,
        /// Lower-bound height.
        min_height: u16,
        /// Upper-bound width.
        max_width: u16,
        /// Upper-bound height.
        max_height: u16,
    },
    /// A range that preserves the aspect ratio: `80**-640*480`.
    ///
    /// The lower bound names a width and *elides* the height with a second asterisk — not
    /// an empty string, which is not legal anywhere in this grammar.
    FixedRatioRange {
        /// Lower-bound width.
        min_width: u16,
        /// Upper-bound width.
        max_width: u16,
        /// Upper-bound height.
        max_height: u16,
    },
}

impl PixelSize {
    /// Parse a `pixel` attribute value.
    ///
    /// Strict, deliberately: no surrounding whitespace, one to five digits per number,
    /// and an upper bound that is not below its lower one — all of which BlueZ enforces,
    /// and none of which a value being read off as a measurement should be lenient about.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let Some((lower, upper)) = value.split_once('-') else {
            let (width, height) = dimension(value)?;
            return Some(Self::Fixed { width, height });
        };
        let (max_width, max_height) = dimension(upper)?;
        // The fixed-ratio form is the only place a height may be absent, and `80**` is
        // its only spelling. Testing for it before the general form is what keeps it from
        // being read as a malformed pair.
        if let Some(min_width) = lower.strip_suffix("**") {
            let min_width = pixel_number(min_width)?;
            return (max_width >= min_width).then_some(Self::FixedRatioRange {
                min_width,
                max_width,
                max_height,
            });
        }
        let (min_width, min_height) = dimension(lower)?;
        (max_width >= min_width && max_height >= min_height).then_some(Self::Range {
            min_width,
            min_height,
            max_width,
            max_height,
        })
    }

    /// Re-spell this size the way BIP writes it.
    ///
    /// A parse/print round trip, used to check the grammar against real listings. **Not**
    /// how a `GetImage` descriptor is built: it once was, on the reasoning that a
    /// responder compares descriptors as strings and so should be handed its own text
    /// back, but that reasoning only holds for a `Fixed` — echoing a *range* asks for a
    /// form nobody holds (#245). [`ChosenImage::descriptor`] names one concrete size.
    #[must_use]
    pub fn as_written(self) -> String {
        match self {
            Self::Fixed { width, height } => format!("{width}*{height}"),
            Self::Range {
                min_width,
                min_height,
                max_width,
                max_height,
            } => format!("{min_width}*{min_height}-{max_width}*{max_height}"),
            Self::FixedRatioRange {
                min_width,
                max_width,
                max_height,
            } => format!("{min_width}**-{max_width}*{max_height}"),
        }
    }

    /// The ceiling this descriptor states.
    ///
    /// What the peer will go up to, and so the honest answer to "how big is the art on
    /// offer" — but **not** a size to ask for. A range is an invitation to name a size
    /// inside it, not a promise of one, and reading the ceiling as the request is what
    /// made a bounded selector discard every ranged offer instead of clamping into it
    /// (#245). [`Self::best_within`] is the question a fetch should ask.
    #[must_use]
    pub const fn largest(self) -> (u16, u16) {
        match self {
            Self::Fixed { width, height } => (width, height),
            Self::Range {
                max_width,
                max_height,
                ..
            }
            | Self::FixedRatioRange {
                max_width,
                max_height,
                ..
            } => (max_width, max_height),
        }
    }

    /// The largest size inside this descriptor with no side over `max_side`.
    ///
    /// The size a `GetImage` should actually name. `None` only when the *smallest* form
    /// the descriptor offers is already over the ceiling — that is the one case where a
    /// bound should discard an offer rather than clamp into it, because there is nothing
    /// inside it we are allowed to ask for.
    ///
    /// The two range forms are clamped differently, and it matters:
    ///
    /// - A [`Self::Range`] is a *box*. BIP lets the client name any pair inside it, and the
    ///   box's corner need not be the picture's shape — Android advertises a hardcoded
    ///   `100*100-1280*1080` over artwork that is square — so each side is clamped on its
    ///   own and the aspect ratio is left to the responder, which is the only party that
    ///   knows it.
    /// - A [`Self::FixedRatioRange`] is a *line*. Its whole point is that the ratio is
    ///   pinned to the ceiling's, so it is scaled to fit; clamping its sides independently
    ///   would name a shape the peer did not offer.
    #[must_use]
    pub fn best_within(self, max_side: u16) -> Option<(u16, u16)> {
        match self {
            // One size on offer: take it, or there is nothing here.
            Self::Fixed { width, height } => {
                if width <= max_side && height <= max_side {
                    Some((width, height))
                } else {
                    None
                }
            }
            Self::Range {
                min_width,
                min_height,
                max_width,
                max_height,
            } => {
                if min_width > max_side || min_height > max_side {
                    return None;
                }
                Some((max_width.min(max_side), max_height.min(max_side)))
            }
            Self::FixedRatioRange {
                min_width,
                max_width,
                max_height,
            } => {
                let longest = max_width.max(max_height);
                if longest <= max_side {
                    return Some((max_width, max_height));
                }
                // Both sides shrink by the same factor, floored so the longer one lands on
                // the ceiling exactly rather than a pixel over it.
                let width = scaled(max_width, max_side, longest);
                let height = scaled(max_height, max_side, longest);
                // The ratio has no lower height to check against — the grammar states one
                // only for the width, and the height follows from it.
                (width >= min_width).then_some((width, height))
            }
        }
    }
}

/// `value * numerator / denominator`, floored.
///
/// `u32` for the product, because two `u16` sides multiplied leave the type the sides are
/// measured in — 65535 × 512 is the case this exists to survive. The quotient comes back
/// inside it: every caller passes a `value` no larger than the `denominator`.
fn scaled(value: u16, numerator: u16, denominator: u16) -> u16 {
    // `max(1)` on the divisor rather than a stated precondition: `0*0` is spellable in the
    // pixel grammar, and a division that cannot panic beats one that must not (rule 7).
    let scaled = u32::from(value) * u32::from(numerator) / u32::from(denominator.max(1));
    u16::try_from(scaled).unwrap_or(u16::MAX)
}

/// A `w*h` pair, both parts mandatory.
fn dimension(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once('*')?;
    Some((pixel_number(w)?, pixel_number(h)?))
}

/// One number from a `pixel` attribute: one to five digits, and nothing else.
///
/// `u16` rather than a range check after the fact, because BIP's own ceiling is 65535 —
/// the type is the bound. The digit test is what rejects `+7`, `0x10` and a leading space,
/// all of which `str::parse` would otherwise wave through or half-accept.
fn pixel_number(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Whether a descriptor is the image the peer stores or one it will produce on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantKind {
    /// `<native>` — the form the peer actually holds.
    Native,
    /// `<variant>` — a form it will transcode to.
    Variant,
}

/// The byte figure a descriptor states.
///
/// The two elements spell it differently and mean different things, which is easy to
/// miss and reads as an absent size: `<native>` carries `size`, the stored object's exact
/// length, and `<variant>` carries `maxsize`, a ceiling on what a transcode will produce
/// (`obexd/client/bip-common.c`, `parse_attrib_native` vs `parse_attrib_variant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteSize {
    /// `size` on a `<native>`: what the object actually weighs.
    Exact(u64),
    /// `maxsize` on a `<variant>`: the most a transcode of it will weigh.
    AtMost(u64),
}

/// One form of an image the peer holds or will produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageVariant {
    /// Stored, or transcoded on request.
    pub kind: VariantKind,
    /// The encoding.
    pub encoding: Encoding,
    /// The size or range, when the descriptor gives one.
    pub pixel: Option<PixelSize>,
    /// What it weighs, when the descriptor says.
    pub size: Option<ByteSize>,
}

/// One form on offer, at the size we would ask it for.
///
/// The pair exists because neither half is a request on its own. A `<variant>` carrying a
/// pixel *range* is an offer to transcode into anything inside it, so the descriptor that
/// goes on the wire has to name one concrete size — spelling the range back verbatim asks
/// for a form no responder holds (#245).
///
/// Only [`ImageProperties::largest_decodable_within`] constructs one, and only from a size
/// it has checked against the peer's own descriptor. That is what keeps "a size the
/// responder never offered" unrepresentable, which BIP requires and which is the whole
/// reason `GetImage` is only reachable after a properties listing has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChosenImage<'a> {
    variant: &'a ImageVariant,
    size: (u16, u16),
}

impl<'a> ChosenImage<'a> {
    /// The form the peer listed.
    #[must_use]
    pub const fn variant(self) -> &'a ImageVariant {
        self.variant
    }

    /// The size that will be asked for, width then height.
    #[must_use]
    pub const fn size(self) -> (u16, u16) {
        self.size
    }

    /// The `Img-Description` XML that asks for exactly this form at exactly this size.
    ///
    /// The encoding is echoed in the responder's own spelling, because BIP compares those
    /// as strings and a peer that wrote `jpeg` would not recognise `JPEG` back. The pixel
    /// field is always the concrete `w*h` chosen here, never a range. The layout is
    /// `obexd/client/bip.c`'s, newlines included.
    #[must_use]
    pub fn descriptor(self) -> String {
        let (width, height) = self.size;
        format!(
            "<image-descriptor version=\"1.0\">\n<image encoding=\"{}\" pixel=\"{width}*{height}\"/>\n</image-descriptor>\n",
            self.variant.encoding.token(),
        )
    }

    /// Whether this form is the cheap one to move, at a size two forms both reach.
    ///
    /// Only reachable since a ranged offer stopped being discarded: Android lists a JPEG
    /// range and a PNG range over the same picture with the same bounds, so clamped into
    /// the airtime ceiling they come out at identical dimensions and the pixel count
    /// cannot separate them. Their cost can — a 512×512 photograph is tens of kilobytes as
    /// JPEG and several hundred as PNG — and this is a link already carrying the audio the
    /// picture belongs to. JPEG alone, rather than a lossy/lossless table: it is the form
    /// AVRCP fixes for the thumbnail, so it is the one every responder is known to produce.
    fn cheap_for_the_radio(self) -> bool {
        matches!(self.variant.encoding.format(), Some(ImageFormat::Jpeg))
    }
}

/// A peer's answer to `x-bt/img-properties`.
///
/// The measurement #75 asks for: whether an iPhone's cover art is genuinely 200×200 —
/// in which case the fetch side is already optimal and the issue closes — or whether it
/// holds something larger that we have simply never asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageProperties {
    /// The handle the listing is for, as the responder echoed it.
    pub handle: Option<String>,
    /// Every form offered, in document order.
    pub variants: Vec<ImageVariant>,
}

impl ImageProperties {
    /// Parse a BIP image-properties document.
    ///
    /// # Errors
    /// [`AudioError::BadImageProperties`] if the XML is malformed or carries no
    /// `image-properties` root.
    pub fn parse(xml: &[u8]) -> Result<Self, AudioError> {
        use quick_xml::events::Event;

        let text =
            std::str::from_utf8(xml).map_err(|e| AudioError::BadImageProperties(e.to_string()))?;
        let mut reader = quick_xml::Reader::from_str(text);
        // Same reasoning as the NVHTTP parser: a bare `&` somewhere in a filename should
        // cost us that attribute, not the whole listing.
        reader.config_mut().allow_dangling_amp = true;

        let mut out = Self::default();
        let mut saw_root = false;
        loop {
            let event = reader
                .read_event()
                .map_err(|e| AudioError::BadImageProperties(e.to_string()))?;
            // `native` and `variant` are conventionally self-closing but need not be, so
            // both start-forms are read the same way.
            let element = match &event {
                Event::Start(e) | Event::Empty(e) => e,
                Event::Eof => break,
                _ => continue,
            };
            match element.name().as_ref() {
                b"image-properties" => {
                    saw_root = true;
                    out.handle = attribute(element, b"handle");
                }
                name @ (b"native" | b"variant") => {
                    let native = name == b"native";
                    let Some(encoding) = attribute(element, b"encoding") else {
                        // A descriptor with no encoding names no image. Skipped rather
                        // than refused: the rest of the listing is still an answer.
                        continue;
                    };
                    // `size` on a native, `maxsize` on a variant — see [`ByteSize`].
                    // Reading only `size` drops every variant's figure silently.
                    let size = if native {
                        attribute(element, b"size")
                            .and_then(|s| s.parse().ok())
                            .map(ByteSize::Exact)
                    } else {
                        attribute(element, b"maxsize")
                            .and_then(|s| s.parse().ok())
                            .map(ByteSize::AtMost)
                    };
                    out.variants.push(ImageVariant {
                        kind: if native {
                            VariantKind::Native
                        } else {
                            VariantKind::Variant
                        },
                        encoding: Encoding::parse(&encoding),
                        pixel: attribute(element, b"pixel")
                            .as_deref()
                            .and_then(PixelSize::parse),
                        size,
                    });
                }
                // `attachment` and anything else: not an image form.
                _ => {}
            }
        }

        if !saw_root {
            return Err(AudioError::BadImageProperties(
                "no image-properties element".into(),
            ));
        }
        Ok(out)
    }

    /// The largest size on offer, and the variant offering it.
    ///
    /// Ignores encodings we cannot decode: a 2048×2048 JPEG 2000 is not a size we can
    /// use — BIP offers that encoding and nothing in this tree reads it — and reporting it
    /// as the ceiling would answer #75's question with a number no code path could reach.
    #[must_use]
    pub fn largest_decodable(&self) -> Option<ChosenImage<'_>> {
        self.largest_decodable_within(u16::MAX, u64::MAX)
    }

    /// The largest form on offer that is worth the airtime to fetch.
    ///
    /// The bound is not about what the panel can draw — the card's art square is over a
    /// thousand pixels on a 4K display, so the screen is never the binding constraint.
    /// It is about the *radio*: this is a Bluetooth link already carrying the audio the
    /// picture belongs to, and cover art is decoration that must not cost a dropout. A
    /// peer offering something enormous should get a thumbnail fetched instead of several
    /// seconds of contended airtime spent on an image nobody asked to wait for.
    ///
    /// `max_bytes` is only checked against a size the descriptor actually states, which in
    /// practice means a `<variant>`'s `maxsize`. A form that declares nothing is judged on
    /// its pixels alone, because guessing a compression ratio would refuse real images.
    ///
    /// `max_side` **bounds the request rather than eliminating the offer** (#245): a
    /// ranged variant is clamped into the ceiling by [`PixelSize::best_within`], and only
    /// a form whose smallest size is already over it is dropped. Judging a range by its
    /// ceiling is what made an Android phone offering `100*100-1280*1080` come back as
    /// "nothing larger than the thumbnail on offer" and land us on its 200×200 native.
    #[must_use]
    pub fn largest_decodable_within(
        &self,
        max_side: u16,
        max_bytes: u64,
    ) -> Option<ChosenImage<'_>> {
        self.variants
            .iter()
            .filter(|v| v.encoding.format().is_some())
            .filter(|v| match v.size {
                // A `maxsize` bounds every size inside a range, so it still holds for the
                // smaller one we clamp to. Loose, but never wrong in the direction that
                // spends airtime.
                Some(ByteSize::Exact(n) | ByteSize::AtMost(n)) => n <= max_bytes,
                None => true,
            })
            .filter_map(|v| {
                Some(ChosenImage {
                    variant: v,
                    size: v.pixel?.best_within(max_side)?,
                })
            })
            .max_by_key(|c| {
                (
                    u32::from(c.size.0) * u32::from(c.size.1),
                    c.cheap_for_the_radio(),
                )
            })
    }
}

/// Read one attribute off an element, as a string.
fn attribute(element: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).trim().to_owned())
}

/// Identify an image by its magic bytes.
///
/// Sniffing rather than trusting a label, because a thumbnail fetch carries no encoding
/// header at all — the profile fixes it as JPEG and responders vary. This covers every
/// format [`ImageFormat`] names that *has* a magic number; TGA has none (its signature is
/// an optional footer), so a TGA cover would have to arrive labelled to be recognised.
#[must_use]
pub fn sniff_format(data: &[u8]) -> Option<ImageFormat> {
    // WebP is a RIFF container: the four-byte length between the two tags means it cannot
    // be written as one flat slice pattern.
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }
    match data {
        [0xFF, 0xD8, 0xFF, ..] => Some(ImageFormat::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [b'G', b'I', b'F', b'8', ..] => Some(ImageFormat::Gif),
        [b'B', b'M', ..] => Some(ImageFormat::Bmp),
        // Both byte orders: `II` is little-endian, `MM` big.
        [b'I', b'I', 0x2A, 0x00, ..] | [b'M', b'M', 0x00, 0x2A, ..] => Some(ImageFormat::Tiff),
        [0x00, 0x00, 0x01, 0x00, ..] => Some(ImageFormat::Ico),
        [b'q', b'o', b'i', b'f', ..] => Some(ImageFormat::Qoi),
        [0x76, 0x2F, 0x31, 0x01, ..] => Some(ImageFormat::OpenExr),
        // Radiance writes either signature.
        _ if data.starts_with(b"#?RADIANCE") || data.starts_with(b"#?RGBE") => {
            Some(ImageFormat::Hdr)
        }
        // Netpbm: `P1`..`P7` covers PBM/PGM/PPM in both ASCII and binary, plus PAM.
        [b'P', kind, ..] if (b'1'..=b'7').contains(kind) => Some(ImageFormat::Pnm),
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
        assert!(session.fetch_thumbnail(handle));
        let mut art = None;
        for reply in replies {
            assert!(session.next_request().is_some(), "session stopped early");
            art = session.feed(&reply)?;
        }
        Ok(art.map(|fetched| match fetched {
            Fetched::Artwork(artwork) => artwork,
            other => panic!("a thumbnail fetch produced {other:?}"),
        }))
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
        assert!(session.fetch_thumbnail("0000001"));
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
        assert!(client.fetch_thumbnail("0000001"));

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
    fn the_first_get_offers_single_response_mode() {
        // AVRCP cover art is always GOEP 2.0 over L2CAP, where SRM support is
        // mandatory (§4.6) and every reference client asks. Not offering it left us in
        // a conversation the responder wasn't having — see the streaming test below.
        let mut client = connected();
        assert!(client.fetch_thumbnail("0000001"));
        let first = ObexPacket::decode(&client.next_request().unwrap(), 0).unwrap();
        assert!(
            first
                .headers
                .contains(&Header::SingleResponseMode(SRM_ENABLE)),
            "the SRM offer belongs on the first GET: {:?}",
            first.headers
        );
    }

    #[test]
    fn a_responder_that_grants_srm_streams_without_further_gets() {
        // The regression this pins: an iPhone that granted SRM streamed its chunks and
        // got a continuation GET back for each one — a protocol violation mid-stream —
        // and dropped the image channel (logs show it sometimes took the ACL link with
        // it, reason 0x13). Under an SRM grant the client must go quiet and consume.
        let image = jpeg(900);
        let mut client = connected();
        assert!(client.fetch_thumbnail("0000001"));
        assert!(client.next_request().is_some(), "the one and only GET");

        // First response grants SRM and starts the stream.
        assert!(client
            .feed(&reply(
                0x90,
                &[],
                vec![
                    Header::SingleResponseMode(SRM_ENABLE),
                    Header::Body(Bytes::copy_from_slice(&image[..300])),
                ],
            ))
            .unwrap()
            .is_none());
        assert!(
            client.next_request().is_none(),
            "a granted SRM means no continuation GETs, ever"
        );

        // The rest arrives unprompted.
        assert!(client
            .feed(&reply(
                0x90,
                &[],
                vec![Header::Body(Bytes::copy_from_slice(&image[300..600]))],
            ))
            .unwrap()
            .is_none());
        assert!(client.next_request().is_none());
        let art = client
            .feed(&reply(
                0xA0,
                &[],
                vec![Header::EndOfBody(Bytes::copy_from_slice(&image[600..]))],
            ))
            .unwrap()
            .expect("artwork");
        assert!(matches!(art, Fetched::Artwork(a) if a.data[..] == image[..]));
        assert!(
            client.is_ready(),
            "and the session is good for the next one"
        );
    }

    #[test]
    fn a_responder_that_ignores_srm_gets_classic_continuations() {
        // Backward compatible by construction: no SRM echo in the first response means
        // request/response, exactly as before the offer existed.
        let mut client = connected();
        assert!(client.fetch_thumbnail("0000001"));
        assert!(client.next_request().is_some());
        client
            .feed(&reply(
                0x90,
                &[],
                vec![Header::Body(Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0x00]))],
            ))
            .unwrap();
        let cont = ObexPacket::decode(&client.next_request().unwrap(), 0).unwrap();
        assert_eq!(cont.code, op::GET_FINAL);
        assert!(
            !cont
                .headers
                .iter()
                .any(|h| matches!(h, Header::SingleResponseMode(_))),
            "the offer is not repeated once the responder has declined it"
        );
    }

    #[test]
    fn a_refused_connect_fails_the_session_rather_than_hanging() {
        let mut session = CoverArtSession::new(0x0400);
        assert!(session.feed(&reply(0xC3, &[0, 0, 0, 0], vec![])).is_err());
        assert_eq!(session.state(), FetchState::Failed);
        assert!(session.next_request().is_none(), "a dead session stops");
        assert!(
            !session.fetch_thumbnail("0000001"),
            "and takes no more requests"
        );
    }

    #[test]
    fn a_handle_that_names_nothing_leaves_the_session_usable() {
        // Plenty of tracks have no art. That must degrade to a text-only card for *that*
        // track — tearing the session down would cost every track after it as well, and
        // rebuilding it takes an SDP query and a channel.
        let mut session = connected();
        assert!(session.fetch_thumbnail("deadbeef"));
        assert!(session.feed(&reply(0xC4, &[], vec![])).is_err());
        assert!(session.is_ready(), "the next track deserves its own chance");
        assert!(session.fetch_thumbnail("0000002"));
    }

    #[test]
    fn a_second_image_reuses_the_session_rather_than_reconnecting() {
        // The reason this is a session at all: reconnecting per track would put an OBEX
        // CONNECT in front of every image, and — worse — a Target strips attribute 8
        // whenever no BIP client is connected, so the gap between tracks would be a
        // window in which handles stop arriving.
        let mut session = connected();
        for handle in ["0000001", "0000002"] {
            assert!(session.fetch_thumbnail(handle));
            let art = session
                .feed(&reply(
                    0xA0,
                    &[],
                    vec![Header::EndOfBody(Bytes::from(jpeg(64)))],
                ))
                .unwrap()
                .expect("artwork");
            assert!(matches!(art, Fetched::Artwork(a) if a.format == ImageFormat::Jpeg));
            assert!(session.is_ready());
        }
    }

    #[test]
    fn a_fetch_while_one_is_already_running_is_refused_not_queued() {
        // A skipped-through album would otherwise build a backlog of images for tracks
        // nobody is on any more.
        let mut session = connected();
        assert!(session.fetch_thumbnail("0000001"));
        assert!(!session.fetch_thumbnail("0000002"));
    }

    #[test]
    fn an_image_in_a_format_we_cannot_decode_is_refused() {
        // Better a text-only card than a decoder failure three layers down.
        //
        // JPEG 2000, because it is a real case rather than a made-up one: BIP's own
        // encoding table offers it (`obexd`, `bip-common.c::encconv_table`) and nothing
        // in this tree can decode it. This used to be WebP, which the pipeline now reads
        // — so that version of the test had stopped testing anything (#87).
        let mut session = connected();
        assert!(session.fetch_thumbnail("x"));
        let err = session.feed(&reply(
            0xA0,
            &[],
            // The JP2 signature box.
            vec![Header::EndOfBody(Bytes::from_static(&[
                0x00, 0x00, 0x00, 0x0C, b'j', b'P', 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
            ]))],
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

        // The rest of what the card can now draw. A thumbnail fetch carries no encoding
        // header, so anything not sniffed here is refused however well the decoder would
        // have coped (#87).
        assert_eq!(
            sniff_format(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageFormat::WebP)
        );
        assert_eq!(
            sniff_format(b"RIFF\x24\x00\x00\x00AVI "),
            None,
            "not every RIFF"
        );
        assert_eq!(sniff_format(b"II\x2a\x00...."), Some(ImageFormat::Tiff));
        assert_eq!(
            sniff_format(b"MM\x00\x2a...."),
            Some(ImageFormat::Tiff),
            "big-endian TIFF is still TIFF"
        );
        assert_eq!(sniff_format(&[0, 0, 1, 0, 1, 0]), Some(ImageFormat::Ico));
        assert_eq!(sniff_format(b"qoif...."), Some(ImageFormat::Qoi));
        assert_eq!(
            sniff_format(&[0x76, 0x2F, 0x31, 0x01]),
            Some(ImageFormat::OpenExr)
        );
        assert_eq!(sniff_format(b"#?RADIANCE\n"), Some(ImageFormat::Hdr));
        assert_eq!(sniff_format(b"P6\n1 1\n255\n"), Some(ImageFormat::Pnm));
        assert_eq!(sniff_format(b"P9 nope"), None, "P8 and up are not Netpbm");

        // JPEG 2000 and WBMP are on BIP's encoding table and are decodable by nothing
        // here, so they must stay unsniffable rather than be guessed at.
        assert_eq!(
            sniff_format(&[0x00, 0x00, 0x00, 0x0C, b'j', b'P', 0x20, 0x20]),
            None
        );
    }

    #[test]
    fn the_thumbnail_get_is_byte_for_byte_what_goes_on_the_wire() {
        // A golden fixture for the exact request the log shows going unanswered
        // (05:04:16.619, handle "1000002"), so the hex the adapter now logs has something
        // to be diffed against rather than reasoned about. Every byte here is a decision
        // that has a wrong answer a responder reacts to by going quiet:
        //
        //   83                  GET, final bit set
        //   002d                length, counting the opcode and itself
        //   cb 00000007         Connection ID — first, and mandatory after a CONNECT that
        //                       carried a Target
        //   97 01               Single Response Mode, enabled; GOEP 2.0 §4.6 makes SRM
        //                       support mandatory over L2CAP, and it goes immediately
        //                       after the Connection ID
        //   42 0010 …00         Type, *ASCII* and null-terminated: "x-bt/img-thm"
        //   30 0013 …0000       Img-Handle (0x30, not Name 0x01), UTF-16 **big-endian**
        //                       and null-terminated
        let mut session = connected();
        assert!(session.fetch_thumbnail("1000002"));
        let request = session.next_request().expect("the get");
        let actual: String = request.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            actual,
            [
                "83002d",
                "cb00000007",
                "9701",
                "420010782d62742f696d672d74686d00",
                "300013",
                "00310030003000300030003000320000",
            ]
            .concat(),
            "the thumbnail GET changed shape; a responder is entitled to notice"
        );
    }

    /// The BIP specification's own example listing, which is also the shape every
    /// responder's is: a `native` form, some `variant`s, and an attachment that is not an
    /// image at all.
    const PROPERTIES_DOC: &[u8] = br#"<image-properties version="1.0" handle="1000001">
<native encoding="JPEG" pixel="1280*1024" size="1048576"/>
<variant encoding="JPEG" pixel="640*480" maxsize="153600"/>
<variant encoding="JPEG" pixel="160*120"/>
<variant encoding="GIF" pixel="80*60-640*480"/>
<attachment content-type="text/plain" name="ABCD1234.txt" size="5120"/>
</image-properties>"#;

    #[test]
    fn a_properties_listing_says_what_the_peer_holds() {
        let props = ImageProperties::parse(PROPERTIES_DOC).unwrap();
        assert_eq!(props.handle.as_deref(), Some("1000001"));
        // Four image forms. The attachment is not one — it is a text file riding along.
        assert_eq!(props.variants.len(), 4);

        let native = &props.variants[0];
        assert_eq!(native.kind, VariantKind::Native);
        assert_eq!(
            native.encoding,
            Encoding::Known(ImageFormat::Jpeg, "JPEG".to_owned())
        );
        assert_eq!(
            native.pixel,
            Some(PixelSize::Fixed {
                width: 1280,
                height: 1024
            })
        );
        // A native says `size` and means it exactly; a variant says `maxsize` and means
        // a ceiling. Reading only `size` drops every variant's figure without a word.
        assert_eq!(native.size, Some(ByteSize::Exact(1_048_576)));
        assert_eq!(props.variants[1].kind, VariantKind::Variant);
        assert_eq!(props.variants[1].size, Some(ByteSize::AtMost(153_600)));
        assert_eq!(props.variants[2].size, None, "and it is optional");
    }

    #[test]
    fn the_pixel_grammar_is_the_three_forms_bip_defines_and_no_others() {
        // Written against `obexd/client/bip-common.c::parse_pixel_range`, which is three
        // anchored regexes: `W*H`, `W*H-W*H`, and `W**-W*H`. An earlier version of this
        // parser was looser in four ways, and every one of them would have quietly
        // mis-recorded the measurement #75 is taken to answer.
        assert_eq!(
            PixelSize::parse("200*200").unwrap(),
            PixelSize::Fixed {
                width: 200,
                height: 200
            }
        );

        // `80*60-640*480` is not an 80×60 image: it is an offer to transcode anywhere up
        // to 640×480, so it is worth its ceiling.
        let range = PixelSize::parse("80*60-640*480").unwrap();
        assert_eq!(
            range,
            PixelSize::Range {
                min_width: 80,
                min_height: 60,
                max_width: 640,
                max_height: 480,
            }
        );
        assert_eq!(range.largest(), (640, 480));

        // The aspect-preserving form elides the lower height with a *second asterisk*,
        // and that is its only spelling.
        assert_eq!(
            PixelSize::parse("80**-640*480").unwrap(),
            PixelSize::FixedRatioRange {
                min_width: 80,
                max_width: 640,
                max_height: 480,
            }
        );
        assert_eq!(PixelSize::parse("80*-640*480"), None, "empty is not elided");
        assert_eq!(PixelSize::parse("80**"), None, "and only inside a range");

        // A ceiling below its floor is not a range. BlueZ rejects it; so must we, or a
        // listing reads as offering something it does not.
        assert_eq!(PixelSize::parse("640*480-80*60"), None);
        assert_eq!(PixelSize::parse("80*480-640*60"), None, "either component");

        // One to five digits, and 65535 is the ceiling the type now enforces.
        assert_eq!(
            PixelSize::parse("65535*65535").unwrap().largest(),
            (65535, 65535)
        );
        assert_eq!(PixelSize::parse("123456*10"), None);
        assert_eq!(PixelSize::parse("70000*10"), None);
        // …and nothing but digits. `str::parse` accepts a leading `+`, and the
        // surrounding whitespace an earlier version trimmed is not in the grammar.
        assert_eq!(PixelSize::parse("+70*10"), None);
        assert_eq!(PixelSize::parse(" 200*200 "), None);
        assert_eq!(PixelSize::parse("garbage"), None);

        // Each form re-spells to the text it was read from, which is what says the three
        // grammars stayed three rather than collapsing into one on the way in.
        for text in ["200*200", "80*60-640*480", "80**-640*480"] {
            assert_eq!(PixelSize::parse(text).unwrap().as_written(), text);
        }
    }

    #[test]
    fn a_ceiling_is_something_a_range_is_clamped_into_not_dropped_for() {
        // #245. The premise this replaces was that "a range is worth as much as its
        // ceiling, because that is what a GetImage against it would return" — which reads
        // an *invitation to name a size* as a promise of one, and so answers a bounded
        // question with "nothing on offer" for every peer that transcodes.
        let fixed = PixelSize::parse("200*200").unwrap();
        assert_eq!(fixed.best_within(512), Some((200, 200)));
        assert_eq!(
            fixed.best_within(128),
            None,
            "a fixed form is take it or leave it"
        );

        // The Android listing from the bench: the ceiling is over ours, the floor is under
        // it, so there is a size inside the offer to ask for.
        let range = PixelSize::parse("100*100-1280*1080").unwrap();
        assert_eq!(range.largest(), (1280, 1080), "still the ceiling it states");
        assert_eq!(range.best_within(512), Some((512, 512)));
        assert_eq!(
            range.best_within(2000),
            Some((1280, 1080)),
            "a ceiling under ours is not something to clamp"
        );

        // Each side on its own, because a range is a box and BIP lets a client name any
        // pair inside it — the corner is not the picture's shape, and Android's `1280*1080`
        // is a constant in its properties builder rather than a measurement of the artwork.
        assert_eq!(
            PixelSize::parse("100*100-1280*400")
                .unwrap()
                .best_within(512),
            Some((512, 400))
        );

        // The one case a bound should still refuse: the *smallest* form on offer is over
        // it, so there is nothing inside the range we are allowed to ask for.
        assert_eq!(
            PixelSize::parse("600*600-1280*1080")
                .unwrap()
                .best_within(512),
            None
        );
    }

    #[test]
    fn the_aspect_pinned_range_is_scaled_rather_than_clamped_per_side() {
        // `80**-640*480` says the ratio is fixed, so the sizes inside it are the scalings
        // of its ceiling and nothing else. Clamping the sides independently would name
        // 512×480 — a 16:15 image the peer never offered.
        let ratio = PixelSize::parse("80**-640*480").unwrap();
        assert_eq!(ratio.best_within(512), Some((512, 384)), "4:3, preserved");
        assert_eq!(
            ratio.best_within(640),
            Some((640, 480)),
            "under the ceiling, untouched"
        );

        // The tall case: the *height* is the binding side, and the width follows it down.
        assert_eq!(
            PixelSize::parse("10**-480*640").unwrap().best_within(512),
            Some((384, 512))
        );

        // And the floor still applies — a lower bound above the ceiling leaves nothing to
        // ask for, exactly as for a plain range.
        assert_eq!(
            PixelSize::parse("600**-1280*1080")
                .unwrap()
                .best_within(512),
            None
        );
    }

    #[test]
    fn a_ranged_offer_is_asked_for_at_one_concrete_size() {
        // The coupled half of #245: a selector that picks a range is no use if the
        // descriptor then spells the range back. `100*100-1280*1080` is a form no
        // responder holds, and asking for it is asking to be refused.
        //
        // Reconstructed from the bench log of 2026-08-08 (an Android phone over LDAC),
        // not a capture — the phone's own bytes were never saved, only the parse.
        let doc = br#"<image-properties version="1.0" handle="7161797">
<native encoding="JPEG" pixel="200*200" size="160000"/>
<variant encoding="JPEG" pixel="100*100-1280*1080"/>
<variant encoding="PNG" pixel="100*100-1280*1080"/>
</image-properties>"#;
        let props = ImageProperties::parse(doc).unwrap();

        let chosen = props.largest_decodable_within(512, 1024 * 1024).unwrap();
        assert_eq!(
            chosen.size(),
            (512, 512),
            "512 a side, where this used to settle for the 200x200 native"
        );
        // JPEG over PNG at the same pixels: the ceiling is an airtime budget, and the two
        // forms do not cost the same to move.
        assert_eq!(
            chosen.variant().encoding,
            Encoding::Known(ImageFormat::Jpeg, "JPEG".to_owned())
        );

        let descriptor = chosen.descriptor();
        assert!(
            descriptor.contains(r#"pixel="512*512""#),
            "one size, not a range: {descriptor}"
        );
        assert!(descriptor.contains(r#"encoding="JPEG""#));
    }

    #[test]
    fn the_largest_size_on_offer_ignores_encodings_we_cannot_decode() {
        // Reporting a 2048×2048 JPEG 2000 as the ceiling answers "how big can the art
        // be?" with a number no code path in this project could ever reach. JPEG 2000
        // because BIP's encoding table offers it and nothing here decodes it — this used
        // to say TIFF, which the pipeline now reads (#87).
        let doc = br#"<image-properties handle="7">
<native encoding="JPEG2000" pixel="2048*2048"/>
<variant encoding="JPEG" pixel="600*600"/>
<variant encoding="JPEG" pixel="200*200"/>
</image-properties>"#;
        let props = ImageProperties::parse(doc).unwrap();
        let chosen = props.largest_decodable().unwrap();
        assert_eq!(chosen.size(), (600, 600));
        assert_eq!(
            chosen.variant().encoding,
            Encoding::Known(ImageFormat::Jpeg, "JPEG".to_owned())
        );
        // …and the one we cannot decode is still recorded, because a capture should say
        // what the peer claimed rather than what we understood of it.
        assert_eq!(
            props.variants[0].encoding,
            Encoding::Unknown("JPEG2000".to_owned())
        );
    }

    #[test]
    fn a_properties_fetch_asks_for_a_different_type_on_the_same_session() {
        // Same channel, same connection id, same refusal rule — only the Type header
        // differs. It carries no image descriptor, which is what makes it safe to ask
        // for from a client that advertises only the linked thumbnail.
        let mut session = connected();
        assert!(session.fetch_properties("1000001"));
        let get = ObexPacket::decode(&session.next_request().unwrap(), 0).unwrap();
        assert!(get
            .headers
            .contains(&Header::Type(TYPE_IMAGE_PROPERTIES.to_owned())));
        assert!(get
            .headers
            .contains(&Header::ImageHandle("1000001".to_owned())));
        assert!(get.headers.contains(&Header::ConnectionId(7)));
    }

    #[test]
    fn a_properties_body_is_parsed_as_xml_rather_than_sniffed_for_jpeg() {
        // The bug this fetch kind exists to make unrepresentable: a properties document
        // fed through `sniff_format` is "not a format we decode", which reads as a peer
        // with no artwork rather than as an answer.
        let mut session = connected();
        assert!(session.fetch_properties("1000001"));
        let _ = session.next_request();
        let fetched = session
            .feed(&reply(
                0xA0,
                &[],
                vec![Header::EndOfBody(Bytes::from_static(PROPERTIES_DOC))],
            ))
            .unwrap()
            .expect("a listing");
        let Fetched::Properties(props) = fetched else {
            panic!("a properties fetch produced artwork");
        };
        assert_eq!(props.variants.len(), 4);
        assert!(session.is_ready(), "and the session is free again");
    }

    #[test]
    fn a_body_that_is_not_a_properties_document_is_a_typed_failure() {
        assert!(matches!(
            ImageProperties::parse(b"<html>nope</html>"),
            Err(AudioError::BadImageProperties(_))
        ));
        // A refused listing must not take the session down with it: the next track still
        // deserves its thumbnail.
        let mut session = connected();
        assert!(session.fetch_properties("1000001"));
        let _ = session.next_request();
        assert!(session
            .feed(&reply(
                0xA0,
                &[],
                vec![Header::EndOfBody(Bytes::from_static(b"<html/>"))]
            ))
            .is_err());
        assert!(session.is_ready());
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
