//! AVRCP: track metadata, playback notifications, transport control, and the cover-art
//! image handle.
//!
//! Role note, because it reads backwards: the *phone* owns the media player, so the phone
//! is the AVRCP **Target** and we are the **Controller**. We ask it for metadata and send
//! it play/pause. We are additionally a Target for one thing only — absolute volume — so
//! the phone's volume rocker reaches us (#69).
//!
//! **Attribute 8 is the point.** It carries a BIP image handle, and fetching that handle
//! over OBEX is the only route to album art. `bluetoothd` parses this exact response and
//! never surfaces the field, which is why owning the stack is what makes artwork
//! reachable (architecture-substrate.md §11.1).

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use castaway_core::{ControlCapabilities, ControlTxn, NowPlaying, PlaybackState};

use crate::avctp::{opcode, AvcFrame, Ctype};
use crate::error::AudioError;

/// The Bluetooth SIG company id that tags an AVRCP vendor-dependent frame.
pub const BT_SIG_COMPANY_ID: u32 = 0x0000_1958;

/// AVRCP PDU identifiers.
pub mod pdu {
    /// Ask which events the peer supports.
    pub const GET_CAPABILITIES: u8 = 0x10;
    /// Read metadata for the current track.
    pub const GET_ELEMENT_ATTRIBUTES: u8 = 0x20;
    /// Ask for the next fragment of a response.
    pub const REQUEST_CONTINUING_RESPONSE: u8 = 0x40;
    /// Abandon a fragmented response, releasing what the peer is holding.
    pub const ABORT_CONTINUING_RESPONSE: u8 = 0x41;
    /// Read length, position and play state.
    pub const GET_PLAY_STATUS: u8 = 0x30;
    /// Subscribe to a change notification.
    pub const REGISTER_NOTIFICATION: u8 = 0x31;
    /// The phone setting *our* volume.
    pub const SET_ABSOLUTE_VOLUME: u8 = 0x50;
}

/// Notification event identifiers.
pub mod event {
    /// Play/pause/stop changed.
    pub const PLAYBACK_STATUS_CHANGED: u8 = 0x01;
    /// The current track changed.
    pub const TRACK_CHANGED: u8 = 0x02;
    /// Playback position moved.
    pub const PLAYBACK_POS_CHANGED: u8 = 0x05;
    /// The peer's volume changed.
    pub const VOLUME_CHANGED: u8 = 0x0D;
}

/// Media attribute identifiers.
pub mod attribute {
    /// Track title.
    pub const TITLE: u32 = 1;
    /// Artist.
    pub const ARTIST: u32 = 2;
    /// Album.
    pub const ALBUM: u32 = 3;
    /// Track number within the album.
    pub const TRACK_NUMBER: u32 = 4;
    /// Total tracks on the album.
    pub const TOTAL_TRACKS: u32 = 5;
    /// Genre.
    pub const GENRE: u32 = 6;
    /// Total playing time in milliseconds.
    pub const PLAYING_TIME: u32 = 7;
    /// **The BIP image handle for cover art.** The one field no OS stack surfaces.
    pub const COVER_ART_HANDLE: u32 = 8;

    /// Everything that can be drawn on a card, with no image handle.
    ///
    /// The set to ask for before a BIP session exists. AOSP's Target *strips* attribute 8
    /// from a response when no cover-art client is connected, so asking for it early buys
    /// nothing — and asking for the text separately means the card appears immediately
    /// rather than waiting on an SDP query and a second L2CAP channel (Q29).
    pub const TEXT: [u32; 7] = [
        TITLE,
        ARTIST,
        ALBUM,
        TRACK_NUMBER,
        TOTAL_TRACKS,
        GENRE,
        PLAYING_TIME,
    ];

    /// Every attribute worth asking for, cover art included.
    pub const ALL: [u32; 8] = [
        TITLE,
        ARTIST,
        ALBUM,
        TRACK_NUMBER,
        TOTAL_TRACKS,
        GENRE,
        PLAYING_TIME,
        COVER_ART_HANDLE,
    ];
}

/// Passthrough operation ids — the transport keys.
pub mod operation {
    /// Play.
    pub const PLAY: u8 = 0x44;
    /// Stop.
    pub const STOP: u8 = 0x45;
    /// Pause.
    pub const PAUSE: u8 = 0x46;
    /// Skip forward.
    pub const FORWARD: u8 = 0x4B;
    /// Skip backward.
    pub const BACKWARD: u8 = 0x4C;
    /// Volume up.
    pub const VOLUME_UP: u8 = 0x41;
    /// Volume down.
    pub const VOLUME_DOWN: u8 = 0x42;
    /// Mute.
    pub const MUTE: u8 = 0x43;
}

/// Map a control transaction onto its passthrough operation id.
///
/// Returns `None` for verbs passthrough cannot express — seek and queue replacement
/// need the browsing channel, and volume is absolute rather than stepwise. Returning
/// `None` rather than a nearest-equivalent matters: a "seek" silently delivered as
/// fast-forward moves the track by an unpredictable amount.
#[must_use]
pub const fn operation_for(txn: &ControlTxn) -> Option<u8> {
    Some(match txn {
        ControlTxn::Play => operation::PLAY,
        ControlTxn::Pause => operation::PAUSE,
        ControlTxn::Stop => operation::STOP,
        ControlTxn::Next => operation::FORWARD,
        ControlTxn::Previous => operation::BACKWARD,
        ControlTxn::Mute(_) => operation::MUTE,
        ControlTxn::Seek(_) | ControlTxn::Volume(_) | ControlTxn::SetQueue { .. } => return None,
        // ControlTxn is #[non_exhaustive]. A verb added upstream is *not* silently
        // mapped to a nearest equivalent — the capability set simply won't offer it,
        // which is the same answer as for seek and for the same reason.
        _ => return None,
    })
}

/// The capabilities an AVRCP peer supporting basic transport gives us.
///
/// Derived from what passthrough can actually express, not from optimism: seek and queue
/// are excluded because [`operation_for`] cannot encode them, so a UI built from this set
/// never offers a control that would silently do nothing.
#[must_use]
pub fn capabilities_for_passthrough() -> ControlCapabilities {
    ControlCapabilities::TRANSPORT | ControlCapabilities::STOP | ControlCapabilities::MUTE
}

/// What the panel may offer, given what the peer's SDP record says it implements.
///
/// The two category bits in `SupportedFeatures` (attribute 0x0311) are what decide
/// whether a command is worth sending: category 1 is Player/Recorder and carries the
/// transport keys, category 2 is Monitor/Amplifier and carries volume. BlueZ gates on
/// exactly these — "only create player if category 1 is supported", and absolute volume
/// on category 2 — and a phone that never claimed a category answers `NOT IMPLEMENTED`.
///
/// `None` for a peer whose record we could not read: assume the usual set rather than
/// leaving the panel with no buttons at all, since the far commoner reason to see nothing
/// here is a record we failed to parse, not a phone that genuinely controls nothing.
#[must_use]
pub fn capabilities_from_features(features: Option<u16>) -> ControlCapabilities {
    use substrate_sdp::avrcp_feature;
    let Some(features) = features else {
        return capabilities_for_passthrough();
    };
    let mut caps = ControlCapabilities::NONE;
    if features & avrcp_feature::CATEGORY_1_PLAYER != 0 {
        caps |= ControlCapabilities::TRANSPORT | ControlCapabilities::STOP;
    }
    if features & avrcp_feature::CATEGORY_2_AMPLIFIER != 0 {
        caps |= ControlCapabilities::MUTE;
    }
    caps
}

/// Whether a passthrough operand marks the key as released rather than pressed.
const RELEASE_BIT: u8 = 0x80;

/// Build the two AV/C frames a passthrough keypress requires.
///
/// **Both are mandatory.** A press with no matching release leaves the peer believing the
/// key is held down; many phones then auto-repeat, so a single tap on "next" skips
/// through the whole album. This returns them together so the pair cannot be separated.
#[must_use]
pub fn passthrough(operation: u8) -> [AvcFrame; 2] {
    let frame = |op: u8| {
        AvcFrame::panel(
            Ctype::Control,
            opcode::PASS_THROUGH,
            Bytes::copy_from_slice(&[op, 0x00]),
        )
    };
    [frame(operation), frame(operation | RELEASE_BIT)]
}

/// Build a vendor-dependent AVRCP command frame.
#[must_use]
pub fn vendor_command(ctype: Ctype, pdu_id: u8, parameters: &[u8]) -> AvcFrame {
    let mut operands = BytesMut::with_capacity(7 + parameters.len());
    // Company id is three bytes, big-endian.
    operands.put_u8(((BT_SIG_COMPANY_ID >> 16) & 0xFF) as u8);
    operands.put_u8(((BT_SIG_COMPANY_ID >> 8) & 0xFF) as u8);
    operands.put_u8((BT_SIG_COMPANY_ID & 0xFF) as u8);
    operands.put_u8(pdu_id);
    operands.put_u8(0x00); // packet type: single
    operands.put_u16(u16::try_from(parameters.len()).unwrap_or(u16::MAX));
    operands.extend_from_slice(parameters);
    AvcFrame::panel(ctype, opcode::VENDOR_DEPENDENT, operands.freeze())
}

/// The `UNIT INFO` response.
///
/// A fixed shape: `0x07` filler, then unit type PANEL in the top five bits, then a
/// 24-bit company id. Trivial to answer and worth answering — BlueZ-as-source asks for it
/// during AVRCP bring-up and waits, so silence here stalls a connection that is otherwise
/// fine.
#[must_use]
pub fn unit_info() -> AvcFrame {
    let mut operands = BytesMut::with_capacity(5);
    operands.put_u8(0x07);
    // Unit type PANEL (0x09) in bits 7..3, unit id 0.
    operands.put_u8(0x09 << 3);
    operands.put_u8(((BT_SIG_COMPANY_ID >> 16) & 0xFF) as u8);
    operands.put_u8(((BT_SIG_COMPANY_ID >> 8) & 0xFF) as u8);
    operands.put_u8((BT_SIG_COMPANY_ID & 0xFF) as u8);
    AvcFrame::panel(Ctype::Stable, opcode::UNIT_INFO, operands.freeze())
}

/// The `SUBUNIT INFO` response: one PANEL subunit, page 0.
#[must_use]
pub fn subunit_info() -> AvcFrame {
    let mut operands = BytesMut::with_capacity(5);
    // Page 0, extension code 7.
    operands.put_u8(0x07);
    // One PANEL subunit: type in bits 7..3, (count - 1) in bits 2..0.
    operands.put_u8(0x09 << 3);
    operands.put_u8(0xFF);
    operands.put_u8(0xFF);
    operands.put_u8(0xFF);
    AvcFrame::panel(Ctype::Stable, opcode::SUBUNIT_INFO, operands.freeze())
}

/// Events our Target will accept a `REGISTER_NOTIFICATION` for.
///
/// Volume is the one that matters: a phone decides whether to hand us absolute-volume
/// control on the strength of this answer, and a Target that never replies does not get
/// offered it.
pub const SUPPORTED_EVENTS: &[u8] = &[event::VOLUME_CHANGED];

/// Build the `GET_CAPABILITIES` response for whatever capability was asked about.
///
/// Two capability ids exist: `0x02` is the company-id list and `0x03` is the event list.
/// Anything else gets an empty list rather than a wrong one.
#[must_use]
pub fn capabilities_response(parameters: &[u8]) -> Vec<u8> {
    const CAP_COMPANY_ID: u8 = 0x02;
    const CAP_EVENTS_SUPPORTED: u8 = 0x03;
    match parameters.first().copied() {
        Some(CAP_COMPANY_ID) => {
            let mut out = vec![CAP_COMPANY_ID, 1];
            out.push(((BT_SIG_COMPANY_ID >> 16) & 0xFF) as u8);
            out.push(((BT_SIG_COMPANY_ID >> 8) & 0xFF) as u8);
            out.push((BT_SIG_COMPANY_ID & 0xFF) as u8);
            out
        }
        Some(CAP_EVENTS_SUPPORTED) => {
            let mut out = vec![
                CAP_EVENTS_SUPPORTED,
                u8::try_from(SUPPORTED_EVENTS.len()).unwrap_or(0),
            ];
            out.extend_from_slice(SUPPORTED_EVENTS);
            out
        }
        other => {
            let id = other.unwrap_or(0);
            vec![id, 0]
        }
    }
}

/// Where a PDU sits in a fragmented exchange.
///
/// Byte 4 of the operands, and it was being skipped entirely — parsed straight past from
/// the pdu id at 3 to the length at 5..7. A fragmented response was therefore read as if
/// its *first fragment* were the whole thing, `parse_element_attributes` returned
/// `Truncated`, and the caller's `if let Ok(..)` dropped it without a word. On any phone
/// whose metadata does not fit in one PDU — a long title, a CJK one, or simply all seven
/// text attributes — the now-playing card stayed permanently blank.
///
/// AVRCP caps a metadata PDU at 512 bytes regardless of the L2CAP MTU, so this is not
/// something a bigger MTU avoids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    /// The whole PDU is here.
    Single,
    /// The first of several; ask for the rest with [`request_continuing`].
    Start,
    /// A middle fragment.
    Continue,
    /// The last fragment.
    End,
}

impl PacketType {
    /// Decode the two-bit field. Values outside 0..=3 cannot occur — it is masked.
    #[must_use]
    const fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            1 => Self::Start,
            2 => Self::Continue,
            3 => Self::End,
            _ => Self::Single,
        }
    }

    /// Whether more fragments are owed after this one.
    #[must_use]
    pub const fn expects_more(self) -> bool {
        matches!(self, Self::Start | Self::Continue)
    }
}

/// A parsed vendor-dependent AVRCP PDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorPdu {
    /// Which PDU.
    pub pdu_id: u8,
    /// Where this sits in a fragmented exchange.
    pub packet_type: PacketType,
    /// Its parameters — this fragment's, if fragmented.
    pub parameters: Bytes,
}

impl VendorPdu {
    /// Parse the operands of a vendor-dependent AV/C frame.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if shorter than the header or than its declared
    /// parameter length.
    pub fn parse(operands: &[u8]) -> Result<Self, AudioError> {
        if operands.len() < 7 {
            return Err(AudioError::Truncated {
                what: "avrcp vendor pdu header",
                need: 7,
                have: operands.len(),
            });
        }
        let len = usize::from(u16::from_be_bytes([operands[5], operands[6]]));
        if operands.len() < 7 + len {
            return Err(AudioError::Truncated {
                what: "avrcp vendor pdu parameters",
                need: 7 + len,
                have: operands.len(),
            });
        }
        Ok(Self {
            pdu_id: operands[3],
            packet_type: PacketType::from_bits(operands[4]),
            parameters: Bytes::copy_from_slice(&operands[7..7 + len]),
        })
    }
}

/// Ask the peer for the next fragment of `pdu_id`'s response.
///
/// Without this a fragmented response can only ever be read as its first fragment: the
/// peer is holding the rest and will not send it unsolicited.
#[must_use]
pub fn request_continuing(pdu_id: u8) -> AvcFrame {
    vendor_command(Ctype::Control, pdu::REQUEST_CONTINUING_RESPONSE, &[pdu_id])
}

/// Tell the peer to stop holding the rest of `pdu_id`'s response.
///
/// Sent when we give up on a reassembly — a peer that is never told keeps the remainder
/// buffered, and some stacks refuse a *new* request for the same PDU while one is
/// outstanding, which would leave metadata broken for the rest of the session.
#[must_use]
pub fn abort_continuing(pdu_id: u8) -> AvcFrame {
    vendor_command(Ctype::Control, pdu::ABORT_CONTINUING_RESPONSE, &[pdu_id])
}

/// Build a `GetElementAttributes` command for the currently playing track.
#[must_use]
pub fn get_element_attributes(attributes: &[u32]) -> AvcFrame {
    let mut params = BytesMut::with_capacity(9 + attributes.len() * 4);
    // Identifier 0 = "the track that is playing now". The field is eight bytes.
    params.put_u64(0);
    params.put_u8(u8::try_from(attributes.len()).unwrap_or(u8::MAX));
    for id in attributes {
        params.put_u32(*id);
    }
    vendor_command(Ctype::Status, pdu::GET_ELEMENT_ATTRIBUTES, &params)
}

/// The attribute ids an inbound `GetElementAttributes` command asked for.
///
/// An empty list in the request means *every* attribute — a zero count is "all", not
/// "none", and reading it the other way answers a head unit with a blank card.
///
/// # Errors
/// [`AudioError::Truncated`] if the parameters are shorter than the identifier and count
/// they declare.
pub fn parse_attribute_request(params: &[u8]) -> Result<Vec<u32>, AudioError> {
    if params.len() < 9 {
        return Err(AudioError::Truncated {
            what: "get element attributes request",
            need: 9,
            have: params.len(),
        });
    }
    let count = usize::from(params[8]);
    if count == 0 {
        return Ok(attribute::ALL.to_vec());
    }
    if params.len() < 9 + count * 4 {
        return Err(AudioError::Truncated {
            what: "get element attributes request ids",
            need: 9 + count * 4,
            have: params.len(),
        });
    }
    Ok((0..count)
        .map(|i| {
            let at = 9 + i * 4;
            u32::from_be_bytes([params[at], params[at + 1], params[at + 2], params[at + 3]])
        })
        .collect())
}

/// The most a metadata response may occupy, so it fits one AVCTP packet.
///
/// AVRCP can fragment a response across packets, and we do not — so the list is truncated
/// rather than overflowed. A card missing its genre beats a response the peer drops.
const MAX_RESPONSE_PARAMETERS: usize = 450;

/// Answer an inbound `GetElementAttributes`, supplying what we know and skipping what we
/// do not.
///
/// **Skipping, not rejecting.** Real GM and Hyundai-Kia head units enumerate attributes
/// 1..=8 unconditionally, so refusing the whole PDU because attribute 8 is in the list —
/// or because some id is unknown — loses the metadata for every attribute that *was*
/// askable. Attribute 8 is one we never supply: we hold an image handle for the peer's
/// image server, which is meaningless as a handle on ours.
#[must_use]
pub fn element_attributes_response(now: &NowPlaying, requested: &[u32]) -> AvcFrame {
    let track = now.track.map(|(number, _)| number);
    let total = now.track.and_then(|(_, total)| total);
    let mut values: Vec<(u32, String)> = Vec::with_capacity(requested.len());
    for id in requested {
        let value = match *id {
            attribute::TITLE => now.title.clone(),
            attribute::ARTIST => now.artist.clone(),
            attribute::ALBUM => now.album.clone(),
            attribute::GENRE => now.genre.clone(),
            attribute::TRACK_NUMBER => track.map(|n| n.to_string()),
            attribute::TOTAL_TRACKS => total.map(|n| n.to_string()),
            // Every numeric attribute in AVRCP is a decimal *string*, milliseconds
            // included.
            attribute::PLAYING_TIME => now
                .duration
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).to_string()),
            // Attribute 8 and anything we do not model: skipped, not refused.
            _ => None,
        };
        if let Some(value) = value {
            values.push((*id, value));
        }
    }

    let mut params = BytesMut::with_capacity(64);
    params.put_u8(0); // count, filled in once the list is known to fit
    let mut written = 0u8;
    for (id, value) in values {
        if params.len() + 8 + value.len() > MAX_RESPONSE_PARAMETERS || written == u8::MAX {
            break;
        }
        params.put_u32(id);
        params.put_u16(106); // UTF-8
        params.put_u16(u16::try_from(value.len()).unwrap_or(u16::MAX));
        params.extend_from_slice(value.as_bytes());
        written += 1;
    }
    params[0] = written;
    vendor_command(Ctype::Stable, pdu::GET_ELEMENT_ATTRIBUTES, &params)
}

/// Build a `RegisterNotification` command.
#[must_use]
pub fn register_notification(event_id: u8, interval_secs: u32) -> AvcFrame {
    let mut params = BytesMut::with_capacity(5);
    params.put_u8(event_id);
    params.put_u32(interval_secs);
    vendor_command(Ctype::Notify, pdu::REGISTER_NOTIFICATION, &params)
}

/// Build a `GetPlayStatus` command.
#[must_use]
pub fn get_play_status() -> AvcFrame {
    vendor_command(Ctype::Status, pdu::GET_PLAY_STATUS, &[])
}

/// What a `GetElementAttributes` response told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackAttributes {
    /// The metadata, as a snapshot ready to hand to the session manager.
    pub now_playing: NowPlaying,
    /// The BIP image handle from attribute 8, if the peer offered one.
    ///
    /// Kept separate from [`NowPlaying`] because it is not renderable — it is a token to
    /// go fetch with, and the artwork it names arrives seconds later over a different
    /// L2CAP channel.
    pub cover_art_handle: Option<String>,
}

/// Parse a `GetElementAttributes` response.
///
/// # Errors
/// [`AudioError::Truncated`] if an attribute's declared length runs past the buffer.
pub fn parse_element_attributes(params: &[u8]) -> Result<TrackAttributes, AudioError> {
    let Some((&count, mut rest)) = params.split_first() else {
        return Err(AudioError::Truncated {
            what: "element attributes count",
            need: 1,
            have: 0,
        });
    };
    let mut out = TrackAttributes::default();
    let mut total_tracks = None;
    let mut track_number = None;

    for _ in 0..count {
        if rest.len() < 8 {
            return Err(AudioError::Truncated {
                what: "element attribute header",
                need: 8,
                have: rest.len(),
            });
        }
        let id = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let charset = u16::from_be_bytes([rest[4], rest[5]]);
        let len = usize::from(u16::from_be_bytes([rest[6], rest[7]]));
        if rest.len() < 8 + len {
            return Err(AudioError::Truncated {
                what: "element attribute value",
                need: 8 + len,
                have: rest.len(),
            });
        }
        let raw = &rest[8..8 + len];
        // Charset 106 is UTF-8 and is what every peer uses in practice, but the field
        // exists, and a lossy decode beats refusing a track because its title is in
        // some legacy encoding.
        let _ = charset;
        let value = String::from_utf8_lossy(raw).into_owned();

        match id {
            attribute::TITLE => out.now_playing.title = non_empty(value),
            attribute::ARTIST => out.now_playing.artist = non_empty(value),
            attribute::ALBUM => out.now_playing.album = non_empty(value),
            attribute::GENRE => out.now_playing.genre = non_empty(value),
            attribute::TRACK_NUMBER => track_number = value.trim().parse::<u32>().ok(),
            attribute::TOTAL_TRACKS => total_tracks = value.trim().parse::<u32>().ok(),
            attribute::PLAYING_TIME => {
                // Milliseconds, as a decimal *string* — every numeric attribute in
                // AVRCP is text, which is easy to miss and reads as a garbage duration.
                out.now_playing.duration =
                    value.trim().parse::<u64>().ok().map(Duration::from_millis);
            }
            attribute::COVER_ART_HANDLE => out.cover_art_handle = non_empty(value),
            _ => {}
        }
        rest = &rest[8 + len..];
    }

    if let Some(n) = track_number {
        out.now_playing.track = Some((n, total_tracks));
    }
    Ok(out)
}

/// Parse a `GetPlayStatus` response into duration, position and state.
///
/// # Errors
/// [`AudioError::Truncated`] if shorter than nine bytes.
pub fn parse_play_status(
    params: &[u8],
) -> Result<(Option<Duration>, Option<Duration>, PlaybackState), AudioError> {
    if params.len() < 9 {
        return Err(AudioError::Truncated {
            what: "play status",
            need: 9,
            have: params.len(),
        });
    }
    let length = u32::from_be_bytes([params[0], params[1], params[2], params[3]]);
    let position = u32::from_be_bytes([params[4], params[5], params[6], params[7]]);
    Ok((
        // 0xFFFFFFFF means "not supported", and rendering it literally is a track
        // 49 days long.
        millis_or_none(length),
        millis_or_none(position),
        playback_state(params[8]),
    ))
}

/// Map an AVRCP play-status byte onto the core state.
#[must_use]
pub const fn playback_state(raw: u8) -> PlaybackState {
    match raw {
        0x00 => PlaybackState::Stopped,
        0x01 => PlaybackState::Playing,
        0x02 => PlaybackState::Paused,
        0x03 => PlaybackState::SeekingForward,
        0x04 => PlaybackState::SeekingBackward,
        _ => PlaybackState::Error,
    }
}

/// Volume as AVRCP carries it: seven bits, `0..=127`.
///
/// Scaling matters and is easy to get wrong: dividing by 128 never reaches 1.0, so a
/// phone at maximum volume would leave us fractionally quiet forever.
#[must_use]
pub fn volume_to_fraction(raw: u8) -> f32 {
    f32::from(raw & 0x7F) / 127.0
}

/// The inverse of [`volume_to_fraction`], clamped into the legal range.
#[must_use]
pub fn fraction_to_volume(fraction: f32) -> u8 {
    let clamped = fraction.clamp(0.0, 1.0);
    // `round` rather than truncate, so a round trip through both functions is stable.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (clamped * 127.0).round() as u8;
    scaled & 0x7F
}

fn non_empty(s: String) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn millis_or_none(raw: u32) -> Option<Duration> {
    (raw != u32::MAX).then(|| Duration::from_millis(u64::from(raw)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use bytes::BufMut;

    use super::*;

    #[test]
    fn the_panel_offers_only_what_the_peer_says_it_implements() {
        // Architecture §11.5 always said capabilities come from the peer's
        // `SupportedFeatures` bitmask "so the UI cannot offer a button the phone will
        // reject". They did not: every phone got the full passthrough set, the button was
        // pressed, an AV/C `NOT IMPLEMENTED` came back, and nothing in the UI reflected it.
        //
        // The two category bits are what BlueZ gates on too — "only create player if
        // category 1 is supported", and absolute volume on category 2.
        use substrate_sdp::avrcp_feature::{CATEGORY_1_PLAYER, CATEGORY_2_AMPLIFIER};

        let player_only = capabilities_from_features(Some(CATEGORY_1_PLAYER));
        assert!(
            player_only.supports(&ControlTxn::Pause),
            "category 1 is transport"
        );
        assert!(
            !player_only.supports(&ControlTxn::Mute(true)),
            "but not volume"
        );

        let amp_only = capabilities_from_features(Some(CATEGORY_2_AMPLIFIER));
        assert!(
            amp_only.supports(&ControlTxn::Mute(true)),
            "category 2 is volume"
        );
        assert!(!amp_only.supports(&ControlTxn::Pause), "but not transport");

        let both = capabilities_from_features(Some(CATEGORY_1_PLAYER | CATEGORY_2_AMPLIFIER));
        assert!(both.supports(&ControlTxn::Pause));
        assert!(both.supports(&ControlTxn::Mute(true)));

        // A peer that claims neither category gets no buttons rather than buttons that
        // do nothing.
        assert_eq!(
            capabilities_from_features(Some(0)),
            ControlCapabilities::NONE
        );
    }

    #[test]
    fn an_unreadable_record_leaves_the_panel_usable() {
        // The commoner reason to have no bitmask is a record we failed to parse, not a
        // phone that genuinely controls nothing — and a panel with no buttons at all is a
        // worse answer to that than a button that might be refused.
        assert_eq!(
            capabilities_from_features(None),
            capabilities_for_passthrough()
        );
    }

    #[test]
    fn the_target_names_the_events_a_phone_may_subscribe_to() {
        // A phone decides whether to hand us absolute-volume control on the strength of
        // this answer. `GET_CAPABILITIES` was defined and never answered, so it heard
        // nothing — and a Target that does not reply does not get offered the feature the
        // whole surface exists for.
        let events = capabilities_response(&[0x03]);
        assert_eq!(events[0], 0x03, "capability id is echoed");
        assert_eq!(usize::from(events[1]), SUPPORTED_EVENTS.len());
        assert!(events[2..].contains(&event::VOLUME_CHANGED));

        // The company-id capability is a different question with a different answer.
        let companies = capabilities_response(&[0x02]);
        assert_eq!(companies[0], 0x02);
        assert_eq!(companies[1], 1);

        // An id we do not know gets an empty list rather than a wrong one.
        assert_eq!(capabilities_response(&[0x7f]), vec![0x7f, 0]);
        assert_eq!(capabilities_response(&[]), vec![0, 0]);
    }

    #[test]
    fn unit_and_subunit_info_answer_as_a_panel() {
        // Both fail `VendorPdu::parse`'s seven-operand minimum, so both used to return
        // silently — and BlueZ-as-source asks for both during AVRCP bring-up and waits.
        let unit = unit_info();
        assert_eq!(unit.opcode, opcode::UNIT_INFO);
        assert_eq!(unit.ctype, Ctype::Stable);
        assert_eq!(unit.operands[0], 0x07);
        assert_eq!(unit.operands[1] >> 3, 0x09, "PANEL");

        let subunit = subunit_info();
        assert_eq!(subunit.opcode, opcode::SUBUNIT_INFO);
        assert_eq!(subunit.ctype, Ctype::Stable);
        assert_eq!(subunit.operands[1] >> 3, 0x09, "PANEL");
    }

    /// Build a `GetElementAttributes` response body the way a phone would.
    fn attributes_response(items: &[(u32, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(u8::try_from(items.len()).unwrap());
        for (id, value) in items {
            buf.put_u32(*id);
            buf.put_u16(106); // UTF-8
            buf.put_u16(u16::try_from(value.len()).unwrap());
            buf.extend_from_slice(value.as_bytes());
        }
        buf
    }

    #[test]
    fn metadata_becomes_a_now_playing_snapshot() {
        let body = attributes_response(&[
            (attribute::TITLE, "Bloom"),
            (attribute::ARTIST, "Beach House"),
            (attribute::ALBUM, "Bloom"),
            (attribute::PLAYING_TIME, "321000"),
            (attribute::TRACK_NUMBER, "1"),
            (attribute::TOTAL_TRACKS, "10"),
        ]);
        let parsed = parse_element_attributes(&body).unwrap();
        assert_eq!(parsed.now_playing.title.as_deref(), Some("Bloom"));
        assert_eq!(parsed.now_playing.artist.as_deref(), Some("Beach House"));
        assert_eq!(
            parsed.now_playing.duration,
            Some(Duration::from_millis(321_000))
        );
        assert_eq!(parsed.now_playing.track, Some((1, Some(10))));
    }

    #[test]
    fn the_cover_art_handle_is_extracted_and_kept_out_of_the_renderable_snapshot() {
        // Attribute 8 is the whole reason this parser exists. It is a token to fetch
        // with, not something to draw, so it must not leak into NowPlaying as text.
        let body = attributes_response(&[
            (attribute::TITLE, "Myth"),
            (attribute::COVER_ART_HANDLE, "0000001"),
        ]);
        let parsed = parse_element_attributes(&body).unwrap();
        assert_eq!(parsed.cover_art_handle.as_deref(), Some("0000001"));
        assert!(parsed.now_playing.artwork.is_none(), "art arrives later");
        assert_eq!(parsed.now_playing.title.as_deref(), Some("Myth"));
    }

    #[test]
    fn numeric_attributes_are_decimal_strings_not_integers() {
        // Every AVRCP attribute value is text, including durations and track numbers.
        // Reading the bytes as a big-endian integer yields a nonsense duration.
        let body = attributes_response(&[(attribute::PLAYING_TIME, "60000")]);
        let parsed = parse_element_attributes(&body).unwrap();
        assert_eq!(parsed.now_playing.duration, Some(Duration::from_secs(60)));
    }

    #[test]
    fn empty_attribute_values_are_absent_rather_than_blank() {
        // A phone that sends an empty artist should render as no artist, not as an empty
        // line under the title.
        let body = attributes_response(&[(attribute::TITLE, "x"), (attribute::ARTIST, "  ")]);
        let parsed = parse_element_attributes(&body).unwrap();
        assert_eq!(parsed.now_playing.artist, None);
    }

    #[test]
    fn a_truncated_attribute_list_is_refused() {
        let mut body = attributes_response(&[(attribute::TITLE, "Bloom")]);
        body.truncate(body.len() - 2);
        assert!(matches!(
            parse_element_attributes(&body),
            Err(AudioError::Truncated { .. })
        ));
    }

    #[test]
    fn play_status_maps_onto_the_core_state_and_rejects_the_unknown_sentinel() {
        let mut body = Vec::new();
        body.put_u32(240_000);
        body.put_u32(15_000);
        body.push(0x01);
        let (len, pos, state) = parse_play_status(&body).unwrap();
        assert_eq!(len, Some(Duration::from_secs(240)));
        assert_eq!(pos, Some(Duration::from_secs(15)));
        assert_eq!(state, PlaybackState::Playing);

        // 0xFFFFFFFF is AVRCP's "not supported". Rendering it literally shows a track
        // 49 days long with a scrubber pinned at zero.
        let mut unknown = Vec::new();
        unknown.put_u32(u32::MAX);
        unknown.put_u32(u32::MAX);
        unknown.push(0x02);
        let (len, pos, state) = parse_play_status(&unknown).unwrap();
        assert_eq!(len, None);
        assert_eq!(pos, None);
        assert_eq!(state, PlaybackState::Paused);
    }

    #[test]
    fn a_passthrough_keypress_is_always_a_press_and_a_release() {
        // Press without release leaves the peer thinking the key is held; phones
        // auto-repeat, so one tap on "next" walks the whole album.
        let [press, release] = passthrough(operation::FORWARD);
        assert_eq!(press.operands[0], operation::FORWARD);
        assert_eq!(release.operands[0], operation::FORWARD | RELEASE_BIT);
        assert_eq!(press.opcode, opcode::PASS_THROUGH);
        assert_eq!(release.opcode, opcode::PASS_THROUGH);
    }

    #[test]
    fn verbs_passthrough_cannot_express_map_to_nothing() {
        // Delivering "seek" as fast-forward moves the track by an unpredictable amount.
        // Refusing is the honest answer, and ControlCapabilities is built to match.
        assert_eq!(operation_for(&ControlTxn::Play), Some(operation::PLAY));
        assert_eq!(operation_for(&ControlTxn::Next), Some(operation::FORWARD));
        assert_eq!(
            operation_for(&ControlTxn::Seek(Duration::from_secs(30))),
            None
        );
        assert_eq!(operation_for(&ControlTxn::Volume(0.5)), None);

        let caps = capabilities_for_passthrough();
        assert!(caps.supports(&ControlTxn::Play));
        assert!(caps.supports(&ControlTxn::Next));
        assert!(
            !caps.supports(&ControlTxn::Seek(Duration::from_secs(1))),
            "the capability set must not offer what operation_for cannot encode"
        );
    }

    #[test]
    fn volume_scaling_reaches_both_ends_of_the_range() {
        // Dividing by 128 never reaches 1.0, so a phone at full volume would leave us
        // permanently a little quiet.
        assert_eq!(volume_to_fraction(0), 0.0);
        assert!((volume_to_fraction(127) - 1.0).abs() < f32::EPSILON);
        assert_eq!(fraction_to_volume(1.0), 127);
        assert_eq!(fraction_to_volume(0.0), 0);
        // Out-of-range input is clamped, not wrapped.
        assert_eq!(fraction_to_volume(2.0), 127);
        assert_eq!(fraction_to_volume(-1.0), 0);
        // Round trip is stable.
        for raw in [0u8, 1, 63, 64, 126, 127] {
            assert_eq!(fraction_to_volume(volume_to_fraction(raw)), raw);
        }
    }

    #[test]
    fn a_vendor_pdu_round_trips_through_its_frame() {
        let frame = get_element_attributes(&attribute::ALL);
        let parsed = VendorPdu::parse(&frame.operands).unwrap();
        assert_eq!(parsed.pdu_id, pdu::GET_ELEMENT_ATTRIBUTES);
        // 8 bytes of identifier, 1 count byte, then four bytes per attribute.
        assert_eq!(parsed.parameters.len(), 8 + 1 + attribute::ALL.len() * 4);
        assert_eq!(parsed.parameters[8] as usize, attribute::ALL.len());
    }

    #[test]
    fn the_attribute_request_asks_for_cover_art() {
        // Omitting attribute 8 from the request is a silent way to never get artwork.
        assert!(attribute::ALL.contains(&attribute::COVER_ART_HANDLE));
    }

    #[test]
    fn an_inbound_request_for_every_attribute_is_answered_with_what_we_have() {
        // Real GM and Hyundai-Kia head units enumerate 1..=8 unconditionally. Refusing
        // the PDU because attribute 8 is in the list loses the metadata for the seven
        // attributes that *were* askable, so unknown ids are skipped instead.
        let mut now = NowPlaying::default();
        now.title = Some("Derezzed".into());
        now.artist = Some("Daft Punk".into());
        now.duration = Some(Duration::from_millis(104_000));
        now.track = Some((7, Some(22)));
        let request = get_element_attributes(&attribute::ALL);
        let requested =
            parse_attribute_request(&VendorPdu::parse(&request.operands).unwrap().parameters)
                .unwrap();
        assert_eq!(requested.len(), 8);

        let response = element_attributes_response(&now, &requested);
        assert_eq!(response.ctype, Ctype::Stable);
        let parsed =
            parse_element_attributes(&VendorPdu::parse(&response.operands).unwrap().parameters)
                .unwrap();
        assert_eq!(parsed.now_playing.title.as_deref(), Some("Derezzed"));
        assert_eq!(parsed.now_playing.artist.as_deref(), Some("Daft Punk"));
        assert_eq!(parsed.now_playing.track, Some((7, Some(22))));
        assert_eq!(
            parsed.now_playing.duration,
            Some(Duration::from_millis(104_000))
        );
        assert!(
            parsed.cover_art_handle.is_none(),
            "attribute 8 is a handle on someone else's image server; we have none to give"
        );
        assert_eq!(parsed.now_playing.album, None, "and nothing is invented");
    }

    #[test]
    fn a_zero_count_in_a_request_means_every_attribute_not_none() {
        // Reading it the other way answers a head unit with a blank card and no error.
        let mut params = Vec::new();
        params.put_u64(0); // track identifier
        params.push(0); // count
        assert_eq!(
            parse_attribute_request(&params).unwrap(),
            attribute::ALL.to_vec()
        );
    }

    #[test]
    fn an_attribute_we_cannot_supply_is_omitted_rather_than_sent_empty() {
        let mut now = NowPlaying::default();
        now.title = Some("x".into());
        let response = element_attributes_response(&now, &[attribute::ARTIST, attribute::TITLE]);
        let params = VendorPdu::parse(&response.operands).unwrap().parameters;
        assert_eq!(params[0], 1, "one attribute, not two with a blank");
    }

    #[test]
    fn a_truncated_inbound_request_is_refused_rather_than_read_as_zero_attributes() {
        assert!(matches!(
            parse_attribute_request(&[0, 0, 0]),
            Err(AudioError::Truncated { .. })
        ));
        let mut short = Vec::new();
        short.put_u64(0);
        short.push(4); // claims four ids and carries none
        assert!(matches!(
            parse_attribute_request(&short),
            Err(AudioError::Truncated { .. })
        ));
    }

    #[test]
    fn a_short_vendor_pdu_is_refused() {
        assert!(matches!(
            VendorPdu::parse(&[0x00, 0x19, 0x58]),
            Err(AudioError::Truncated { .. })
        ));
    }
}
