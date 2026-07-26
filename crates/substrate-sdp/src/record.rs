//! Service records: what we publish about ourselves, and how to read a peer's.

use std::collections::BTreeMap;

use crate::element::DataElement;
use crate::uuid::Uuid;

/// Attribute identifiers used by the records we publish and read.
pub mod attr {
    /// Service record handle.
    pub const SERVICE_RECORD_HANDLE: u16 = 0x0000;
    /// The service classes this record implements.
    pub const SERVICE_CLASS_ID_LIST: u16 = 0x0001;
    /// The protocol stack the service is reached over — where our PSM lives.
    pub const PROTOCOL_DESCRIPTOR_LIST: u16 = 0x0004;
    /// Browse groups this record appears in.
    pub const BROWSE_GROUP_LIST: u16 = 0x0005;
    /// Language base offsets for the human-readable attributes.
    pub const LANGUAGE_BASE_ATTRIBUTE_ID_LIST: u16 = 0x0006;
    /// Profiles and their versions.
    pub const BLUETOOTH_PROFILE_DESCRIPTOR_LIST: u16 = 0x0009;
    /// Extra protocol stacks — **where the peer's cover-art PSM is published**.
    pub const ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST: u16 = 0x000D;
    /// Human-readable service name (primary language).
    pub const SERVICE_NAME: u16 = 0x0100;
    /// Profile-specific supported-features bitmask.
    pub const SUPPORTED_FEATURES: u16 = 0x0311;
}

/// A2DP sink supported-features bits.
pub mod a2dp_features {
    /// Headphone.
    pub const HEADPHONE: u16 = 1 << 0;
    /// Speaker — what a room's PA system is.
    pub const SPEAKER: u16 = 1 << 1;
    /// Recorder.
    pub const RECORDER: u16 = 1 << 2;
    /// Amplifier.
    pub const AMPLIFIER: u16 = 1 << 3;
}

/// AVRCP supported-features bits, as they appear in each role's record.
///
/// The two roles do **not** share a bit layout past the four category bits, which is the
/// trap: bit 6 is "supports browsing" in a Controller record, and the cover-art bits sit
/// at 7, 8 and 9 — one per BIP operation. Claiming bit 6 for cover art advertises a
/// browsing channel we do not implement *and* leaves the peer with no reason to send an
/// image handle, so it fails in both directions at once (Q29).
pub mod avrcp_features {
    /// Category 1: player/recorder. The category that carries play/pause/next/previous.
    pub const CATEGORY_1_PLAYER: u16 = 1 << 0;
    /// Category 2: monitor/amplifier. The category absolute volume belongs to.
    pub const CATEGORY_2_AMPLIFIER: u16 = 1 << 1;
    /// Controller supports browsing. We do not — the browsing channel is a second AVCTP
    /// connection and its own PDU set — so this is named to be avoided, not set.
    pub const CONTROLLER_SUPPORTS_BROWSING: u16 = 1 << 6;
    /// Controller supports `GetImageProperties`.
    pub const CONTROLLER_GET_IMAGE_PROPERTIES: u16 = 1 << 7;
    /// Controller supports `GetImage` — the full-size fetch, which needs an image
    /// descriptor negotiated per request.
    pub const CONTROLLER_GET_IMAGE: u16 = 1 << 8;
    /// Controller supports `GetLinkedThumbnail`. **The bit that gets us album art**:
    /// without a cover-art bit the peer has no reason to include an image handle in its
    /// metadata responses, and this is the one operation we implement.
    pub const CONTROLLER_GET_LINKED_THUMBNAIL: u16 = 1 << 9;
    /// Target supports cover art. A different bit again, in a different record — ours is
    /// a volume-only Target, so it is named for reading a peer's record, not writing our
    /// own.
    pub const TARGET_SUPPORTS_COVER_ART: u16 = 1 << 8;
}

/// A parsed or constructed SDP service record.
///
/// `BTreeMap` rather than a `Vec` of pairs because SDP requires attributes to be returned
/// in ascending id order, and getting that wrong upsets stacks that binary-search the
/// response instead of scanning it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceRecord {
    attributes: BTreeMap<u16, DataElement>,
}

impl ServiceRecord {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set an attribute.
    #[must_use]
    pub fn with(mut self, id: u16, value: DataElement) -> Self {
        self.attributes.insert(id, value);
        self
    }

    /// Read an attribute.
    #[must_use]
    pub fn get(&self, id: u16) -> Option<&DataElement> {
        self.attributes.get(&id)
    }

    /// Every attribute, in ascending id order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &DataElement)> {
        self.attributes.iter().map(|(k, v)| (*k, v))
    }

    /// The record's handle, if it has one.
    #[must_use]
    pub fn handle(&self) -> Option<u32> {
        self.get(attr::SERVICE_RECORD_HANDLE)
            .and_then(DataElement::as_uint)
            .and_then(|v| u32::try_from(v).ok())
    }

    /// Whether this record advertises `class`.
    ///
    /// Compares expanded UUIDs, so a peer that spells the class out in 128 bits still
    /// matches a search for the 16-bit form.
    #[must_use]
    pub fn has_class(&self, class: Uuid) -> bool {
        self.get(attr::SERVICE_CLASS_ID_LIST)
            .and_then(DataElement::as_sequence)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter_map(DataElement::as_uuid)
                    .any(|u| u.as_bytes() == class.as_bytes())
            })
    }

    /// Find the L2CAP PSM in a protocol descriptor list attribute.
    ///
    /// A descriptor list is a sequence of protocol stacks, each a sequence starting with
    /// the protocol UUID. For L2CAP the next element is the PSM.
    #[must_use]
    pub fn l2cap_psm(&self, attribute: u16) -> Option<u16> {
        self.l2cap_psm_under(attribute, None)
    }

    /// The same, restricted to the stack whose layer above L2CAP is `protocol`.
    ///
    /// This is the lookup that finds the peer's cover-art channel — the one piece of the
    /// album-art path no OS stack will hand us — and the restriction is not optional. An
    /// `AdditionalProtocolDescriptorList` routinely holds *several* stacks: an iPhone
    /// publishes its AVCTP **browsing** channel there too, and it comes first. Taking the
    /// first PSM in the list therefore opens a browsing channel and speaks OBEX at it,
    /// which fails in a way that looks like the peer having no cover art at all (Q29).
    ///
    /// BlueZ does exactly this test — `sdp_uuid_to_proto(...) == OBEX_UUID`, then
    /// `sdp_get_proto_port` on the *same* stack.
    #[must_use]
    pub fn l2cap_psm_under(&self, attribute: u16, protocol: Option<Uuid>) -> Option<u16> {
        let list = self.get(attribute)?.as_sequence()?;
        // `ProtocolDescriptorList` *is* one stack — a sequence of layers, each starting
        // with a protocol UUID. `AdditionalProtocolDescriptorList` is a sequence of those.
        // The shapes are told apart by looking one level down rather than by which
        // attribute was asked for, since peers are not always tidy about it.
        let is_single_stack = list
            .first()
            .and_then(DataElement::as_sequence)
            .and_then(<[DataElement]>::first)
            .and_then(DataElement::as_uuid)
            .is_some();
        if is_single_stack {
            let layers: Vec<&DataElement> = list.iter().collect();
            return Self::psm_from_stack(&layers, protocol);
        }
        for stack in list {
            let Some(layers) = stack.as_sequence() else {
                continue;
            };
            let layers: Vec<&DataElement> = layers.iter().collect();
            if let Some(psm) = Self::psm_from_stack(&layers, protocol) {
                return Some(psm);
            }
        }
        None
    }

    /// The PSM of one protocol stack, if its layers are the ones asked for.
    fn psm_from_stack(layers: &[&DataElement], protocol: Option<Uuid>) -> Option<u16> {
        let matches_protocol = protocol.is_none_or(|wanted| {
            layers.iter().skip(1).any(|layer| {
                layer
                    .as_sequence()
                    .and_then(|parts| parts.first())
                    .and_then(DataElement::as_uuid)
                    .is_some_and(|uuid| uuid.as_bytes() == wanted.as_bytes())
            })
        });
        if !matches_protocol {
            return None;
        }
        for layer in layers {
            let parts = layer.as_sequence()?;
            let Some(proto) = parts.first().and_then(DataElement::as_uuid) else {
                continue;
            };
            if proto.as_bytes() == Uuid::L2CAP.as_bytes() {
                if let Some(psm) = parts.get(1).and_then(DataElement::as_uint) {
                    return u16::try_from(psm).ok();
                }
            }
        }
        None
    }

    /// The service name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self.get(attr::SERVICE_NAME) {
            Some(DataElement::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// A protocol stack entry: `[uuid, params…]`.
fn proto(uuid: Uuid, params: Vec<DataElement>) -> DataElement {
    let mut items = vec![DataElement::Uuid(uuid)];
    items.extend(params);
    DataElement::Sequence(items)
}

/// A profile descriptor: `[profile uuid, version]`.
fn profile(uuid: Uuid, version: u16) -> DataElement {
    // The version is a fixed-width 16-bit field, not a number that may shrink.
    DataElement::Sequence(vec![DataElement::Uuid(uuid), DataElement::Uint16(version)])
}

/// The common tail every record we publish shares.
fn base(handle: u32, name: &str, classes: Vec<Uuid>) -> ServiceRecord {
    ServiceRecord::new()
        .with(attr::SERVICE_RECORD_HANDLE, DataElement::Uint32(handle))
        .with(attr::SERVICE_CLASS_ID_LIST, DataElement::uuid_seq(classes))
        .with(
            attr::BROWSE_GROUP_LIST,
            DataElement::uuid_seq([Uuid::PUBLIC_BROWSE_ROOT]),
        )
        .with(
            attr::LANGUAGE_BASE_ATTRIBUTE_ID_LIST,
            // English, UTF-8, base 0x0100 — the offsets ServiceName is relative to.
            // All three are fixed-width 16-bit. The encoding (106) is the trap: it fits
            // in a byte, so a value-derived width emits a five-byte triplet where the
            // reader expects six, and a strict parser then cannot resolve ServiceName.
            DataElement::Sequence(vec![
                DataElement::Uint16(0x656e),
                DataElement::Uint16(106),
                DataElement::Uint16(0x0100),
            ]),
        )
        .with(attr::SERVICE_NAME, DataElement::Text(name.to_owned()))
}

/// The A2DP Sink record: "this device accepts an audio stream on PSM 0x0019".
#[must_use]
pub fn a2dp_sink(handle: u32, name: &str) -> ServiceRecord {
    base(handle, name, vec![Uuid::AUDIO_SINK])
        .with(
            attr::PROTOCOL_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![
                proto(Uuid::L2CAP, vec![DataElement::Uint16(0x0019)]),
                proto(Uuid::AVDTP, vec![DataElement::Uint16(0x0103)]),
            ]),
        )
        .with(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![profile(Uuid::ADVANCED_AUDIO_DISTRIBUTION, 0x0103)]),
        )
        .with(
            attr::SUPPORTED_FEATURES,
            DataElement::Uint16(a2dp_features::SPEAKER),
        )
}

/// The AVRCP **Controller** record.
///
/// The role split is counter-intuitive and worth stating once: the *phone* owns the media
/// player, so the phone is the AVRCP Target and we are the Controller — we are the end
/// that asks for metadata and sends play/pause. A sink that publishes only a Target record
/// gets no metadata at all.
///
/// The cover-art bit matters: without
/// [`avrcp_features::CONTROLLER_GET_LINKED_THUMBNAIL`] the peer will not include an image
/// handle in its metadata responses, and album art silently never arrives.
///
/// The class list carries **both** `0x110E` and `0x110F`. AVRCP defines the generic
/// A/V Remote Control class as part of every role's record, and a peer that searches for
/// `0x110E` — which is what the profile says to search for — finds nothing in a record
/// that lists only the role-specific class.
#[must_use]
pub fn avrcp_controller(handle: u32, name: &str) -> ServiceRecord {
    base(
        handle,
        name,
        vec![Uuid::AV_REMOTE_CONTROL, Uuid::AV_REMOTE_CONTROL_CONTROLLER],
    )
    .with(
        attr::PROTOCOL_DESCRIPTOR_LIST,
        DataElement::Sequence(vec![
            proto(Uuid::L2CAP, vec![DataElement::Uint16(0x0017)]),
            proto(Uuid::AVCTP, vec![DataElement::Uint16(0x0104)]),
        ]),
    )
    .with(
        attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
        DataElement::Sequence(vec![profile(Uuid::AV_REMOTE_CONTROL, 0x0106)]),
    )
    .with(
        attr::SUPPORTED_FEATURES,
        // Only the thumbnail operation is claimed. Advertising `GetImage` as well
        // would be free right up until a peer offered a handle we then asked for the
        // wrong way — the full-image form needs an image descriptor negotiated per
        // request, and we do not send one.
        DataElement::Uint16(
            avrcp_features::CATEGORY_1_PLAYER | avrcp_features::CONTROLLER_GET_LINKED_THUMBNAIL,
        ),
    )
}

/// The AVRCP **Target** record.
///
/// We publish this *as well* as the controller record, for one reason: absolute volume.
/// The phone's volume rocker sends `SetAbsoluteVolume` to a Target, so being only a
/// Controller means the rocker does nothing — which is the behaviour Q24 chose against.
#[must_use]
pub fn avrcp_target(handle: u32, name: &str) -> ServiceRecord {
    base(handle, name, vec![Uuid::AV_REMOTE_CONTROL_TARGET])
        .with(
            attr::PROTOCOL_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![
                proto(Uuid::L2CAP, vec![DataElement::Uint16(0x0017)]),
                proto(Uuid::AVCTP, vec![DataElement::Uint16(0x0104)]),
            ]),
        )
        .with(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![profile(Uuid::AV_REMOTE_CONTROL, 0x0106)]),
        )
        .with(
            attr::SUPPORTED_FEATURES,
            DataElement::Uint16(avrcp_features::CATEGORY_2_AMPLIFIER),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_sink_record_advertises_avdtp_on_the_right_psm() {
        let rec = a2dp_sink(0x0001_0000, "Hackerspace TV");
        assert!(rec.has_class(Uuid::AUDIO_SINK));
        assert_eq!(rec.l2cap_psm(attr::PROTOCOL_DESCRIPTOR_LIST), Some(0x0019));
        assert_eq!(rec.name(), Some("Hackerspace TV"));
        assert_eq!(rec.handle(), Some(0x0001_0000));
    }

    #[test]
    fn we_publish_controller_and_target_because_they_do_different_jobs() {
        // Controller = we drive the phone's player (metadata, play/pause).
        // Target = the phone's volume rocker reaches us (absolute volume, Q24).
        // Publishing only one of these silently loses half the feature.
        let ct = avrcp_controller(1, "x");
        let tg = avrcp_target(2, "x");
        assert!(ct.has_class(Uuid::AV_REMOTE_CONTROL_CONTROLLER));
        assert!(
            ct.has_class(Uuid::AV_REMOTE_CONTROL),
            "0x110E is what the profile says to search for; omitting it hides the record"
        );
        assert!(tg.has_class(Uuid::AV_REMOTE_CONTROL_TARGET));
        assert!(!ct.has_class(Uuid::AV_REMOTE_CONTROL_TARGET));

        let ct_features = ct.get(attr::SUPPORTED_FEATURES).unwrap().as_uint().unwrap();
        assert_ne!(
            ct_features & u64::from(avrcp_features::CONTROLLER_GET_LINKED_THUMBNAIL),
            0,
            "without the cover-art bit the peer never sends an image handle"
        );
        assert_eq!(
            ct_features & u64::from(avrcp_features::CONTROLLER_SUPPORTS_BROWSING),
            0,
            "bit 6 is browsing, which we do not implement — claiming it is its own bug"
        );
        let tg_features = tg.get(attr::SUPPORTED_FEATURES).unwrap().as_uint().unwrap();
        assert_ne!(
            tg_features & u64::from(avrcp_features::CATEGORY_2_AMPLIFIER),
            0,
            "category 2 is the one absolute volume lives in"
        );
    }

    #[test]
    fn a_class_matches_whichever_width_the_peer_wrote_it_in() {
        let long = Uuid::long(*Uuid::AUDIO_SINK.as_bytes());
        let rec =
            ServiceRecord::new().with(attr::SERVICE_CLASS_ID_LIST, DataElement::uuid_seq([long]));
        assert!(
            rec.has_class(Uuid::AUDIO_SINK),
            "128-bit spelling must match a 16-bit search"
        );
    }

    #[test]
    fn the_cover_art_psm_is_found_in_the_additional_descriptor_list() {
        // This is the shape a phone's AVRCP Target record actually has, and reading it
        // is the only way to reach album art — bluetoothd never surfaces it.
        let peer = ServiceRecord::new()
            .with(
                attr::SERVICE_CLASS_ID_LIST,
                DataElement::uuid_seq([Uuid::AV_REMOTE_CONTROL_TARGET]),
            )
            .with(
                attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
                DataElement::Sequence(vec![DataElement::Sequence(vec![
                    proto(Uuid::L2CAP, vec![DataElement::Uint(0x1005)]),
                    proto(Uuid::OBEX, vec![]),
                ])]),
            );
        assert_eq!(
            peer.l2cap_psm_under(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST, Some(Uuid::OBEX)),
            Some(0x1005)
        );
    }

    #[test]
    fn the_browsing_channel_is_not_mistaken_for_the_image_server() {
        // The bug that made an iPhone look like it had no cover art. A real AVRCP 1.6
        // Target publishes *two* extra stacks and browsing comes first, so taking the
        // first PSM in the list opens a browsing channel and then speaks OBEX at it.
        let peer = ServiceRecord::new()
            .with(
                attr::SERVICE_CLASS_ID_LIST,
                DataElement::uuid_seq([Uuid::AV_REMOTE_CONTROL_TARGET]),
            )
            .with(
                attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
                DataElement::Sequence(vec![
                    DataElement::Sequence(vec![
                        proto(Uuid::L2CAP, vec![DataElement::Uint(0x001B)]),
                        proto(Uuid::AVCTP, vec![DataElement::Uint16(0x0104)]),
                    ]),
                    DataElement::Sequence(vec![
                        proto(Uuid::L2CAP, vec![DataElement::Uint(0x1005)]),
                        proto(Uuid::OBEX, vec![]),
                    ]),
                ]),
            );
        assert_eq!(
            peer.l2cap_psm_under(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST, Some(Uuid::OBEX)),
            Some(0x1005),
            "the image server is the stack whose layer above L2CAP is OBEX"
        );
        // …and the unrestricted lookup is exactly the trap, which is why the caller that
        // wants cover art must not use it.
        assert_eq!(
            peer.l2cap_psm(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST),
            Some(0x001B)
        );
    }

    #[test]
    fn a_peer_with_browsing_but_no_image_server_yields_nothing() {
        // Plenty of senders publish a browsing channel and no cover art. That must read
        // as "no picture", not as a PSM to go and fail against.
        let peer = ServiceRecord::new().with(
            attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![DataElement::Sequence(vec![
                proto(Uuid::L2CAP, vec![DataElement::Uint(0x001B)]),
                proto(Uuid::AVCTP, vec![DataElement::Uint16(0x0104)]),
            ])]),
        );
        assert_eq!(
            peer.l2cap_psm_under(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST, Some(Uuid::OBEX)),
            None
        );
    }

    #[test]
    fn a_record_without_the_attribute_yields_no_psm() {
        assert_eq!(
            a2dp_sink(1, "x").l2cap_psm(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST),
            None
        );
    }

    #[test]
    fn attributes_come_back_in_ascending_id_order() {
        // SDP requires it, and stacks that binary-search the response depend on it.
        let ids: Vec<u16> = a2dp_sink(1, "x").iter().map(|(id, _)| id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }
}
