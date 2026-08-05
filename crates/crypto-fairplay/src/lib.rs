//! # crypto-fairplay
//!
//! FairPlay-SAP is the gate AirPlay puts in front of *mirroring*: it protects the
//! session key that decrypts the video stream. This crate speaks the `/fp-setup`
//! handshake and stops, precisely, where the published record stops.
//!
//! ## What "the captured tables" actually means
//!
//! The handshake and the key derivation are two different problems, and conflating them
//! made this look far more blocked than it is.
//!
//! **The handshake is a table lookup and an echo, and both are implemented here.**
//! SETUP1 is a 16-byte request whose byte 14 selects one of four canned 142-byte
//! replies. SETUP2 is a 164-byte request answered with a fixed 12-byte header followed
//! by a verbatim copy of the request's last 20 bytes — there is *no* cryptography in it
//! at all. The "~568 bytes" this was once described as needing is not a message size;
//! it is exactly `4 × 142`, those four replies, and they are byte-identical across
//! UxPlay, shairport-sync, pyatv, airplay2-receiver and RPiPlay.
//!
//! **The derivation lives in `crypto-playfair`, not here** (#39, closed). Turning the
//! 72-byte `ekey` from the RTSP `SETUP` plist into the 16-byte AES key needs the OmgHax
//! table set (~99 KiB) and about 1200 lines of algorithm, and the licence decision
//! `docs/airplay-research.md` §5.3 lays out went the way of a separate crate: the material
//! is GPL where this workspace is MIT. `crypto_playfair::decrypt_key` is what production
//! calls (`proto-airplay/src/session.rs`), and it is settled against airplay2-receiver's
//! 20 published `(key message, ekey, expected key)` vectors across all four modes.
//!
//! [`FairPlaySession::decrypt_ekey`] here is therefore a **stub on a dead path**, kept so
//! the boundary is explicit rather than absent; it is the only thing in this crate that
//! returns [`FairPlayError::KeyDerivationUnavailable`].
//!
//! None of this blocks AirPlay 1 audio: that key arrives RSA-wrapped in the `ANNOUNCE` SDP
//! and never touches FairPlay.
//!
//! This is also distinct from **FairPlay Streaming** (content DRM) — a wall we don't touch.
#![forbid(unsafe_code)]

use thiserror::Error;

/// FairPlay handshake errors.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum FairPlayError {
    /// The body wasn't a recognizable fp-setup message.
    #[error("malformed fp-setup message: {0}")]
    Malformed(&'static str),

    /// The version byte was not the v3 flow we speak.
    #[error("unsupported fp-setup version {0:#04x} (only v3 is implemented)")]
    UnsupportedVersion(u8),

    /// The sequence byte named a message we have no reply for.
    #[error("unknown fp-setup sequence byte {0:#04x}")]
    UnknownSequence(u8),

    /// The message-type byte was not the setup type. Both `/fp-setup` messages carry
    /// type 1; anything else means we are looking at a message this crate does not
    /// model, and guessing at its shape would be worse than saying so.
    #[error("unexpected fp-setup message type {0:#04x} (expected {SETUP_MESSAGE_TYPE:#04x})")]
    UnexpectedType(u8),

    /// SETUP1's mode byte selects one of four canned replies; this was not one of them.
    #[error("fp-setup mode {0} is out of range (expected 0..={max})", max = MODE_COUNT - 1)]
    UnknownMode(u8),

    /// `decrypt_ekey` was called before the SETUP2 message that carries its input.
    #[error("no fp-setup SETUP2 key message has been received")]
    NoKeyMessage,

    /// Unwrapping the AES key needs the OmgHax tables, which are not transcribed here.
    /// This is the *only* remaining FairPlay boundary — see the crate docs.
    #[error("FairPlay key derivation is not implemented (needs the OmgHax tables)")]
    KeyDerivationUnavailable,
}

/// The magic every fp-setup body starts with.
const FPLY_MAGIC: &[u8; 4] = b"FPLY";
/// The only FairPlay version this speaks. UxPlay rejects anything else outright.
const VERSION_V3: u8 = 0x03;
/// Number of canned SETUP1 replies, indexed by the request's mode byte.
const MODE_COUNT: usize = 4;
/// Every SETUP1 reply is this long.
const SETUP1_REPLY_LEN: usize = 142;
/// A SETUP1 request is exactly this long.
const SETUP1_REQUEST_LEN: usize = 16;
/// A SETUP2 request is exactly this long.
const SETUP2_REQUEST_LEN: usize = 164;
/// A SETUP2 reply is a 12-byte header plus a 20-byte echo.
const SETUP2_REPLY_LEN: usize = 32;
/// How many trailing bytes of the SETUP2 request are echoed back.
const SETUP2_ECHO_LEN: usize = 20;

/// Byte offsets into an fp-setup request header.
mod offset {
    /// The version byte. **Not** the mode byte — reading the mode from here is the
    /// mistake this constant exists to name.
    pub const VERSION: usize = 4;
    /// The message type (1 for the setup messages).
    pub const TYPE: usize = 5;
    /// Which message in the sequence this is: 1 = SETUP1, 3 = SETUP2.
    pub const SEQUENCE: usize = 6;
    /// SETUP1's mode selector, which indexes the reply table.
    pub const MODE: usize = 14;
}

/// The message-type byte both setup messages carry.
const SETUP_MESSAGE_TYPE: u8 = 1;
/// Sequence byte for the first setup message.
const SEQ_SETUP1: u8 = 1;
/// Sequence byte for the second setup message.
const SEQ_SETUP2: u8 = 3;

const SETUP1_REPLIES: [[u8; SETUP1_REPLY_LEN]; MODE_COUNT] = [
    [
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x82, 0x02, 0x00, 0x0f,
        0x9f, 0x3f, 0x9e, 0x0a, 0x25, 0x21, 0xdb, 0xdf, 0x31, 0x2a, 0xb2, 0xbf, 0xb2, 0x9e, 0x8d,
        0x23, 0x2b, 0x63, 0x76, 0xa8, 0xc8, 0x18, 0x70, 0x1d, 0x22, 0xae, 0x93, 0xd8, 0x27, 0x37,
        0xfe, 0xaf, 0x9d, 0xb4, 0xfd, 0xf4, 0x1c, 0x2d, 0xba, 0x9d, 0x1f, 0x49, 0xca, 0xaa, 0xbf,
        0x65, 0x91, 0xac, 0x1f, 0x7b, 0xc6, 0xf7, 0xe0, 0x66, 0x3d, 0x21, 0xaf, 0xe0, 0x15, 0x65,
        0x95, 0x3e, 0xab, 0x81, 0xf4, 0x18, 0xce, 0xed, 0x09, 0x5a, 0xdb, 0x7c, 0x3d, 0x0e, 0x25,
        0x49, 0x09, 0xa7, 0x98, 0x31, 0xd4, 0x9c, 0x39, 0x82, 0x97, 0x34, 0x34, 0xfa, 0xcb, 0x42,
        0xc6, 0x3a, 0x1c, 0xd9, 0x11, 0xa6, 0xfe, 0x94, 0x1a, 0x8a, 0x6d, 0x4a, 0x74, 0x3b, 0x46,
        0xc3, 0xa7, 0x64, 0x9e, 0x44, 0xc7, 0x89, 0x55, 0xe4, 0x9d, 0x81, 0x55, 0x00, 0x95, 0x49,
        0xc4, 0xe2, 0xf7, 0xa3, 0xf6, 0xd5, 0xba,
    ],
    [
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x82, 0x02, 0x01, 0xcf,
        0x32, 0xa2, 0x57, 0x14, 0xb2, 0x52, 0x4f, 0x8a, 0xa0, 0xad, 0x7a, 0xf1, 0x64, 0xe3, 0x7b,
        0xcf, 0x44, 0x24, 0xe2, 0x00, 0x04, 0x7e, 0xfc, 0x0a, 0xd6, 0x7a, 0xfc, 0xd9, 0x5d, 0xed,
        0x1c, 0x27, 0x30, 0xbb, 0x59, 0x1b, 0x96, 0x2e, 0xd6, 0x3a, 0x9c, 0x4d, 0xed, 0x88, 0xba,
        0x8f, 0xc7, 0x8d, 0xe6, 0x4d, 0x91, 0xcc, 0xfd, 0x5c, 0x7b, 0x56, 0xda, 0x88, 0xe3, 0x1f,
        0x5c, 0xce, 0xaf, 0xc7, 0x43, 0x19, 0x95, 0xa0, 0x16, 0x65, 0xa5, 0x4e, 0x19, 0x39, 0xd2,
        0x5b, 0x94, 0xdb, 0x64, 0xb9, 0xe4, 0x5d, 0x8d, 0x06, 0x3e, 0x1e, 0x6a, 0xf0, 0x7e, 0x96,
        0x56, 0x16, 0x2b, 0x0e, 0xfa, 0x40, 0x42, 0x75, 0xea, 0x5a, 0x44, 0xd9, 0x59, 0x1c, 0x72,
        0x56, 0xb9, 0xfb, 0xe6, 0x51, 0x38, 0x98, 0xb8, 0x02, 0x27, 0x72, 0x19, 0x88, 0x57, 0x16,
        0x50, 0x94, 0x2a, 0xd9, 0x46, 0x68, 0x8a,
    ],
    [
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x82, 0x02, 0x02, 0xc1,
        0x69, 0xa3, 0x52, 0xee, 0xed, 0x35, 0xb1, 0x8c, 0xdd, 0x9c, 0x58, 0xd6, 0x4f, 0x16, 0xc1,
        0x51, 0x9a, 0x89, 0xeb, 0x53, 0x17, 0xbd, 0x0d, 0x43, 0x36, 0xcd, 0x68, 0xf6, 0x38, 0xff,
        0x9d, 0x01, 0x6a, 0x5b, 0x52, 0xb7, 0xfa, 0x92, 0x16, 0xb2, 0xb6, 0x54, 0x82, 0xc7, 0x84,
        0x44, 0x11, 0x81, 0x21, 0xa2, 0xc7, 0xfe, 0xd8, 0x3d, 0xb7, 0x11, 0x9e, 0x91, 0x82, 0xaa,
        0xd7, 0xd1, 0x8c, 0x70, 0x63, 0xe2, 0xa4, 0x57, 0x55, 0x59, 0x10, 0xaf, 0x9e, 0x0e, 0xfc,
        0x76, 0x34, 0x7d, 0x16, 0x40, 0x43, 0x80, 0x7f, 0x58, 0x1e, 0xe4, 0xfb, 0xe4, 0x2c, 0xa9,
        0xde, 0xdc, 0x1b, 0x5e, 0xb2, 0xa3, 0xaa, 0x3d, 0x2e, 0xcd, 0x59, 0xe7, 0xee, 0xe7, 0x0b,
        0x36, 0x29, 0xf2, 0x2a, 0xfd, 0x16, 0x1d, 0x87, 0x73, 0x53, 0xdd, 0xb9, 0x9a, 0xdc, 0x8e,
        0x07, 0x00, 0x6e, 0x56, 0xf8, 0x50, 0xce,
    ],
    [
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x82, 0x02, 0x03, 0x90,
        0x01, 0xe1, 0x72, 0x7e, 0x0f, 0x57, 0xf9, 0xf5, 0x88, 0x0d, 0xb1, 0x04, 0xa6, 0x25, 0x7a,
        0x23, 0xf5, 0xcf, 0xff, 0x1a, 0xbb, 0xe1, 0xe9, 0x30, 0x45, 0x25, 0x1a, 0xfb, 0x97, 0xeb,
        0x9f, 0xc0, 0x01, 0x1e, 0xbe, 0x0f, 0x3a, 0x81, 0xdf, 0x5b, 0x69, 0x1d, 0x76, 0xac, 0xb2,
        0xf7, 0xa5, 0xc7, 0x08, 0xe3, 0xd3, 0x28, 0xf5, 0x6b, 0xb3, 0x9d, 0xbd, 0xe5, 0xf2, 0x9c,
        0x8a, 0x17, 0xf4, 0x81, 0x48, 0x7e, 0x3a, 0xe8, 0x63, 0xc6, 0x78, 0x32, 0x54, 0x22, 0xe6,
        0xf7, 0x8e, 0x16, 0x6d, 0x18, 0xaa, 0x7f, 0xd6, 0x36, 0x25, 0x8b, 0xce, 0x28, 0x72, 0x6f,
        0x66, 0x1f, 0x73, 0x88, 0x93, 0xce, 0x44, 0x31, 0x1e, 0x4b, 0xe6, 0xc0, 0x53, 0x51, 0x93,
        0xe5, 0xef, 0x72, 0xe8, 0x68, 0x62, 0x33, 0x72, 0x9c, 0x22, 0x7d, 0x82, 0x0c, 0x99, 0x94,
        0x45, 0xd8, 0x92, 0x46, 0xc8, 0xc3, 0x59,
    ],
];

const SETUP2_REPLY_HEADER: [u8; 12] = [
    0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
];

/// A parsed `/fp-setup` request.
///
/// Which message this is comes from the sender's own sequence byte, not from a counter
/// the receiver keeps. That matters: the sender is authoritative about what it just
/// sent, and a receiver that tracks its own position gets permanently out of step the
/// moment a message is retried or a session is reused on the same connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FpSetupRequest {
    /// The first message: 16 bytes, selecting one of the canned replies.
    Setup1 {
        /// The reply-table index, from byte 14.
        mode: Mode,
    },
    /// The second message: 164 bytes, carrying the key material the derivation needs.
    Setup2 {
        /// The whole request, retained because `decrypt_ekey` consumes it later.
        key_message: Box<[u8; SETUP2_REQUEST_LEN]>,
    },
}

/// SETUP1's mode selector: an index into the four canned replies, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode(u8);

impl Mode {
    /// Validate a mode byte at the boundary, so downstream indexing cannot panic.
    ///
    /// # Errors
    /// [`FairPlayError::UnknownMode`] if it does not name one of the four replies.
    pub fn new(raw: u8) -> Result<Self, FairPlayError> {
        if usize::from(raw) < MODE_COUNT {
            Ok(Self(raw))
        } else {
            Err(FairPlayError::UnknownMode(raw))
        }
    }

    /// The raw byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl FpSetupRequest {
    /// Parse an fp-setup body.
    ///
    /// # Errors
    /// [`FairPlayError`] if the magic, version, length or sequence byte is wrong.
    pub fn parse(body: &[u8]) -> Result<Self, FairPlayError> {
        // Long enough to hold every header field we are about to read, including the
        // mode byte at offset 14 — checked once, here, rather than at each access.
        if body.len() <= offset::MODE {
            return Err(FairPlayError::Malformed("fp-setup body too short"));
        }
        if &body[..4] != FPLY_MAGIC {
            return Err(FairPlayError::Malformed("missing FPLY magic"));
        }
        if body[offset::VERSION] != VERSION_V3 {
            return Err(FairPlayError::UnsupportedVersion(body[offset::VERSION]));
        }
        if body[offset::TYPE] != SETUP_MESSAGE_TYPE {
            return Err(FairPlayError::UnexpectedType(body[offset::TYPE]));
        }
        match body[offset::SEQUENCE] {
            SEQ_SETUP1 => {
                if body.len() != SETUP1_REQUEST_LEN {
                    return Err(FairPlayError::Malformed("SETUP1 must be 16 bytes"));
                }
                Ok(Self::Setup1 {
                    mode: Mode::new(body[offset::MODE])?,
                })
            }
            SEQ_SETUP2 => {
                let key_message: [u8; SETUP2_REQUEST_LEN] = body
                    .try_into()
                    .map_err(|_| FairPlayError::Malformed("SETUP2 must be 164 bytes"))?;
                Ok(Self::Setup2 {
                    key_message: Box::new(key_message),
                })
            }
            other => Err(FairPlayError::UnknownSequence(other)),
        }
    }
}

/// The AirPlay FairPlay-SAP handshake driver.
///
/// Pure: feed it the bodies the RTSP `/fp-setup` handler received, get back the bodies
/// to send. The only state worth keeping is the SETUP2 key message, because that is the
/// only thing a later step needs — so its presence *is* the state, rather than a stage
/// counter that could disagree with it.
#[derive(Debug, Clone, Default)]
pub struct FairPlaySession {
    key_message: Option<Box<[u8; SETUP2_REQUEST_LEN]>>,
}

impl FairPlaySession {
    /// Start a fresh handshake.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one `/fp-setup` POST body, returning the reply body to send.
    ///
    /// # Errors
    /// [`FairPlayError`] if the body does not parse. Both setup messages are fully
    /// implemented; nothing in this method reaches an unimplemented boundary.
    pub fn handle(&mut self, body: &[u8]) -> Result<Vec<u8>, FairPlayError> {
        match FpSetupRequest::parse(body)? {
            FpSetupRequest::Setup1 { mode } => Ok(SETUP1_REPLIES[usize::from(mode.get())].to_vec()),
            FpSetupRequest::Setup2 { key_message } => {
                let mut reply = Vec::with_capacity(SETUP2_REPLY_LEN);
                reply.extend_from_slice(&SETUP2_REPLY_HEADER);
                reply.extend_from_slice(&key_message[SETUP2_REQUEST_LEN - SETUP2_ECHO_LEN..]);
                self.key_message = Some(key_message);
                Ok(reply)
            }
        }
    }

    /// The 164-byte SETUP2 key message, once it has arrived.
    #[must_use]
    pub fn key_message(&self) -> Option<&[u8; SETUP2_REQUEST_LEN]> {
        self.key_message.as_deref()
    }

    /// Unwrap the 72-byte `ekey` from the RTSP `SETUP` plist into the 16-byte AES key
    /// that decrypts the mirroring stream.
    ///
    /// **This is the one remaining FairPlay boundary.** See the crate documentation for
    /// what it would take and why it is not merely a matter of capturing more bytes.
    ///
    /// # Errors
    /// [`FairPlayError::NoKeyMessage`] if SETUP2 has not been seen, otherwise
    /// [`FairPlayError::KeyDerivationUnavailable`].
    pub fn decrypt_ekey(&self, _ekey: &[u8; 72]) -> Result<[u8; 16], FairPlayError> {
        // Ordered so the caller's own mistake is reported before ours.
        if self.key_message.is_none() {
            return Err(FairPlayError::NoKeyMessage);
        }
        Err(FairPlayError::KeyDerivationUnavailable)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A well-formed SETUP1 request selecting `mode`.
    fn setup1(mode: u8) -> Vec<u8> {
        let mut v = vec![0u8; SETUP1_REQUEST_LEN];
        v[..4].copy_from_slice(FPLY_MAGIC);
        v[offset::VERSION] = VERSION_V3;
        v[offset::TYPE] = 1;
        v[offset::SEQUENCE] = SEQ_SETUP1;
        v[offset::MODE] = mode;
        v
    }

    /// A well-formed SETUP2 request whose last 20 bytes are recognisable.
    fn setup2() -> Vec<u8> {
        let mut v = vec![0u8; SETUP2_REQUEST_LEN];
        v[..4].copy_from_slice(FPLY_MAGIC);
        v[offset::VERSION] = VERSION_V3;
        v[offset::TYPE] = 1;
        v[offset::SEQUENCE] = SEQ_SETUP2;
        for (i, b) in v[SETUP2_REQUEST_LEN - SETUP2_ECHO_LEN..]
            .iter_mut()
            .enumerate()
        {
            *b = 0xA0 + u8::try_from(i).unwrap_or(0);
        }
        v
    }

    #[test]
    fn every_canned_reply_is_142_bytes_with_the_right_header() {
        // The table is transcribed from published constants, so its shape is worth
        // asserting rather than trusting: each reply is `FPLY 03 01 02 .. 82 02 <mode>`.
        for (mode, reply) in SETUP1_REPLIES.iter().enumerate() {
            assert_eq!(reply.len(), SETUP1_REPLY_LEN);
            assert_eq!(&reply[..4], FPLY_MAGIC);
            assert_eq!(reply[offset::VERSION], VERSION_V3);
            assert_eq!(reply[13], u8::try_from(mode).unwrap());
        }
    }

    #[test]
    fn setup1_returns_the_reply_its_mode_byte_selects() {
        for mode in 0..u8::try_from(MODE_COUNT).unwrap() {
            let mut s = FairPlaySession::new();
            let reply = s.handle(&setup1(mode)).unwrap();
            assert_eq!(reply, SETUP1_REPLIES[usize::from(mode)].to_vec());
        }
    }

    #[test]
    fn the_mode_byte_is_at_offset_14_not_offset_4() {
        // Offset 4 is the *version*. Reading the mode from there meant every request
        // looked like mode 3, and the version check was reading the magic's last byte.
        let mut a = setup1(0);
        a[offset::MODE] = 2;
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&a).unwrap(), SETUP1_REPLIES[2].to_vec());
        assert_eq!(a[offset::VERSION], VERSION_V3, "offset 4 stays the version");
    }

    #[test]
    fn setup2_is_a_header_and_an_echo_with_no_crypto_in_it() {
        let mut s = FairPlaySession::new();
        let req = setup2();
        let reply = s.handle(&req).unwrap();
        assert_eq!(reply.len(), SETUP2_REPLY_LEN);
        assert_eq!(&reply[..12], &SETUP2_REPLY_HEADER);
        assert_eq!(&reply[12..], &req[SETUP2_REQUEST_LEN - SETUP2_ECHO_LEN..]);
    }

    #[test]
    fn setup2_retains_the_key_message_the_derivation_needs() {
        let mut s = FairPlaySession::new();
        assert!(s.key_message().is_none());
        let req = setup2();
        s.handle(&req).unwrap();
        assert_eq!(s.key_message().unwrap().as_slice(), req.as_slice());
    }

    #[test]
    fn which_message_this_is_comes_from_the_sender_not_from_a_counter() {
        // SETUP2 arriving first is answered as SETUP2. A receiver tracking its own
        // stage would have called this out of order and refused it.
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&setup2()).unwrap().len(), SETUP2_REPLY_LEN);
        // And SETUP1 after it is still SETUP1, not "the next one".
        assert_eq!(s.handle(&setup1(1)).unwrap().len(), SETUP1_REPLY_LEN);
    }

    #[test]
    fn rejects_a_body_that_is_not_fp_setup() {
        let mut s = FairPlaySession::new();
        assert_eq!(
            s.handle(b"nope-nope-nope-nope"),
            Err(FairPlayError::Malformed("missing FPLY magic"))
        );
    }

    #[test]
    fn rejects_a_body_too_short_to_hold_its_own_header() {
        let mut s = FairPlaySession::new();
        assert_eq!(
            s.handle(b"FPLY\x03"),
            Err(FairPlayError::Malformed("fp-setup body too short"))
        );
    }

    #[test]
    fn version_other_than_v3_is_refused() {
        let mut body = setup1(0);
        body[offset::VERSION] = 0x01;
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&body), Err(FairPlayError::UnsupportedVersion(1)));
    }

    #[test]
    fn an_out_of_range_mode_is_refused_rather_than_indexing_the_table() {
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&setup1(7)), Err(FairPlayError::UnknownMode(7)));
    }

    #[test]
    fn a_message_type_we_do_not_model_is_refused_rather_than_guessed_at() {
        let mut body = setup1(0);
        body[offset::TYPE] = 4;
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&body), Err(FairPlayError::UnexpectedType(4)));
    }

    #[test]
    fn an_unknown_sequence_byte_is_named_in_the_error() {
        let mut body = setup1(0);
        body[offset::SEQUENCE] = 9;
        let mut s = FairPlaySession::new();
        assert_eq!(s.handle(&body), Err(FairPlayError::UnknownSequence(9)));
    }

    #[test]
    fn a_setup1_of_the_wrong_length_is_refused() {
        let mut body = setup1(0);
        body.push(0);
        let mut s = FairPlaySession::new();
        assert_eq!(
            s.handle(&body),
            Err(FairPlayError::Malformed("SETUP1 must be 16 bytes"))
        );
    }

    #[test]
    fn key_derivation_is_the_only_boundary_left() {
        let mut s = FairPlaySession::new();
        // Before SETUP2 the caller's own mistake is reported first.
        assert_eq!(s.decrypt_ekey(&[0u8; 72]), Err(FairPlayError::NoKeyMessage));
        s.handle(&setup2()).unwrap();
        assert_eq!(
            s.decrypt_ekey(&[0u8; 72]),
            Err(FairPlayError::KeyDerivationUnavailable)
        );
    }
}
