//! Extended Inquiry Response: the payload that makes us visible to phones.
//!
//! A controller answers an inquiry with an FHS packet — address, clock, class of device —
//! and nothing else. A name is not in there. Hosts that want one issue a separate
//! `RemoteNameRequest`, which is a second round trip against a device that may already
//! have gone back to streaming, so the EIR exists to fold that answer into the inquiry
//! response itself.
//!
//! Whether skipping it is *fatal* depends entirely on the peer, which is what makes the
//! bug it causes so confusing:
//!
//! - **BlueZ** (Linux) does the follow-up name request. A device with no EIR shows up
//!   unnamed and then acquires a name a moment later — visibly, in the UI.
//! - **Android** builds its picker from the inquiry response alone. No name in the EIR
//!   means no usable entry, so the receiver is *discoverable and invisible* — the worst
//!   failure shape available, because the radio is provably working.
//!
//! The payload is a sequence of AD structures — `len`, `type`, `data`, where `len` counts
//! the type byte — zero-padded to [`Eir::CAPACITY`]. Same encoding BLE advertising uses,
//! which is why the type constants look familiar; the transport is the only difference.
//!
//! The service-class list is a *claim*, and a peer is entitled to believe it without
//! asking SDP. Advertising a UUID we do not serve is therefore worse than advertising
//! nothing: it invites a connection to a profile that will then refuse. Keep this list
//! and the SDP records built from the same source.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::HciError;

/// AD type: the local name, complete.
const AD_COMPLETE_LOCAL_NAME: u8 = 0x09;
/// AD type: the local name, truncated to fit.
const AD_SHORTENED_LOCAL_NAME: u8 = 0x08;
/// AD type: complete list of 16-bit service class UUIDs.
const AD_COMPLETE_UUIDS16: u8 = 0x03;

/// Bytes of overhead each AD structure pays: the length byte and the type byte.
const AD_OVERHEAD: usize = 2;

/// A growable extended inquiry response, bounded by the controller's fixed field.
///
/// Built by chaining; the byte budget is checked as each structure is appended, so an
/// over-long payload is a typed error here rather than a truncated advertisement that
/// half-parses on the far end.
#[derive(Debug, Clone, Default)]
pub struct Eir {
    buf: BytesMut,
}

impl Eir {
    /// The controller's EIR field is a fixed 240 bytes, zero-padded.
    pub const CAPACITY: usize = 240;

    /// An empty response.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(Self::CAPACITY),
        }
    }

    /// Bytes written so far, before padding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been added yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append one AD structure.
    ///
    /// # Errors
    /// [`HciError::TooLong`] if the structure does not fit the remaining budget.
    fn push(&mut self, what: &'static str, ad_type: u8, data: &[u8]) -> Result<(), HciError> {
        let needed = data.len() + AD_OVERHEAD;
        // The length byte counts the type byte plus the data, so the data itself can
        // never exceed 254 even when the budget would otherwise allow it.
        let fits = u8::try_from(data.len() + 1).is_ok() && needed <= self.remaining();
        if !fits {
            return Err(HciError::TooLong {
                what,
                len: self.buf.len() + needed,
                max: Self::CAPACITY,
            });
        }
        // Cast is guarded by `fits` above: data.len() + 1 is known to fit a u8.
        #[allow(clippy::cast_possible_truncation)]
        self.buf.put_u8((data.len() + 1) as u8);
        self.buf.put_u8(ad_type);
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// Room left for further structures.
    fn remaining(&self) -> usize {
        Self::CAPACITY.saturating_sub(self.buf.len())
    }

    /// Advertise the complete list of 16-bit service class UUIDs we serve.
    ///
    /// "Complete" is a promise that SDP holds nothing else, so pass every class that
    /// appears in a published record. An empty list writes nothing rather than an empty
    /// structure, which some parsers read as "this device offers no services at all".
    ///
    /// # Errors
    /// [`HciError::TooLong`] if the list does not fit.
    pub fn with_uuids16(mut self, uuids: &[u16]) -> Result<Self, HciError> {
        if uuids.is_empty() {
            return Ok(self);
        }
        let mut data = Vec::with_capacity(uuids.len() * 2);
        for uuid in uuids {
            // Little-endian on the wire, unlike the big-endian form SDP uses. Getting
            // this backwards yields a device advertising services nobody recognises.
            data.extend_from_slice(&uuid.to_le_bytes());
        }
        self.push("EIR service class list", AD_COMPLETE_UUIDS16, &data)?;
        Ok(self)
    }

    /// Advertise the friendly name, shortening it if it does not fit.
    ///
    /// The spec provides a distinct type for a truncated name precisely so a peer can
    /// tell "this is the whole name" from "there is more"; silently emitting a cut-down
    /// name as [`AD_COMPLETE_LOCAL_NAME`] is a lie a UI cannot recover from. If there is
    /// not even room for one character, the name is dropped entirely — better an unnamed
    /// device that BlueZ can chase with a name request than a malformed structure that
    /// aborts the whole EIR parse.
    #[must_use]
    pub fn with_name(mut self, name: &str) -> Self {
        let budget = self.remaining().saturating_sub(AD_OVERHEAD);
        let (ad_type, text) = if name.len() <= budget {
            (AD_COMPLETE_LOCAL_NAME, name)
        } else {
            // Truncate on a character boundary — a name split mid-codepoint is invalid
            // UTF-8 and renders as a replacement character on the phone, if it renders.
            let end = name
                .char_indices()
                .map(|(i, c)| i + c.len_utf8())
                .take_while(|end| *end <= budget)
                .last()
                .unwrap_or(0);
            (AD_SHORTENED_LOCAL_NAME, &name[..end])
        };
        if text.is_empty() {
            return self;
        }
        // Cannot fail: `text` was measured against the remaining budget just above.
        let _ = self.push("EIR local name", ad_type, text.as_bytes());
        self
    }

    /// The assembled structures, unpadded. The command encoder pads to the fixed width.
    #[must_use]
    pub fn finish(self) -> Bytes {
        self.buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_name_and_uuid_list_serialize_as_ad_structures() {
        let eir = Eir::new()
            .with_uuids16(&[0x110B, 0x110E])
            .unwrap()
            .with_name("castaway")
            .finish();
        assert_eq!(
            &eir[..],
            &[
                // len=5 (type + two little-endian UUIDs), complete 16-bit UUID list
                0x05, 0x03, 0x0B, 0x11, 0x0E, 0x11,
                // len=9 (type + 8 chars), complete local name
                0x09, 0x09, b'c', b'a', b's', b't', b'a', b'w', b'a', b'y',
            ][..]
        );
    }

    #[test]
    fn an_empty_uuid_list_writes_nothing() {
        // An empty "complete list" structure reads as "I serve nothing", which is a
        // stronger and more wrong claim than staying silent.
        assert!(Eir::new().with_uuids16(&[]).unwrap().is_empty());
    }

    #[test]
    fn an_over_long_name_is_marked_shortened() {
        let name = "n".repeat(300);
        let eir = Eir::new().with_name(&name).finish();
        assert_eq!(eir[1], AD_SHORTENED_LOCAL_NAME);
        // Fills the field exactly: 238 characters plus the two overhead bytes.
        assert_eq!(eir.len(), Eir::CAPACITY);
        assert_eq!(usize::from(eir[0]), Eir::CAPACITY - 1);
    }

    #[test]
    fn a_shortened_name_is_cut_on_a_character_boundary() {
        // Three bytes per character, so the budget does not divide evenly — the naive
        // slice would land mid-codepoint and panic.
        let name = "☃".repeat(100);
        let eir = Eir::new().with_name(&name).finish();
        let text = std::str::from_utf8(&eir[2..]).expect("must remain valid UTF-8");
        assert_eq!(text.chars().count(), 79);
        assert!(eir.len() <= Eir::CAPACITY);
    }

    #[test]
    fn the_name_never_displaces_the_service_list() {
        // Order matters: the UUID list is what a peer uses to decide we are worth
        // connecting to, so it is written first and a long name yields to it.
        let eir = Eir::new()
            .with_uuids16(&[0x110B])
            .unwrap()
            .with_name(&"n".repeat(300))
            .finish();
        assert_eq!(&eir[..4], &[0x03, 0x03, 0x0B, 0x11]);
        assert_eq!(eir.len(), Eir::CAPACITY);
    }

    #[test]
    fn a_uuid_list_that_cannot_fit_is_an_error() {
        let many: Vec<u16> = (0..200).collect();
        assert!(matches!(
            Eir::new().with_uuids16(&many),
            Err(HciError::TooLong { .. })
        ));
    }

    #[test]
    fn a_name_with_no_room_left_is_dropped_rather_than_malformed() {
        // 118 UUIDs fill 238 of the 240 bytes, and an AD structure costs 2 before it
        // says anything — so there is room for a header and no name to put under it.
        let uuids: Vec<u16> = (0..118).collect();
        let eir = Eir::new().with_uuids16(&uuids).unwrap();
        let before = eir.len();
        assert_eq!(before, 238);
        assert_eq!(eir.with_name("castaway").finish().len(), before);
    }

    #[test]
    fn a_name_squeezed_into_the_last_bytes_is_still_well_formed() {
        // One UUID fewer leaves exactly two bytes of name budget.
        let uuids: Vec<u16> = (0..117).collect();
        let eir = Eir::new()
            .with_uuids16(&uuids)
            .unwrap()
            .with_name("castaway");
        let bytes = eir.finish();
        assert_eq!(bytes.len(), Eir::CAPACITY);
        assert_eq!(
            &bytes[236..],
            &[0x03, AD_SHORTENED_LOCAL_NAME, b'c', b'a'][..]
        );
    }
}
