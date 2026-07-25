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
pub mod avrcp_features {
    /// Category 1: player/recorder. The category that carries play/pause/next/previous.
    pub const CATEGORY_1_PLAYER: u16 = 1 << 0;
    /// Category 2: monitor/amplifier. The category absolute volume belongs to.
    pub const CATEGORY_2_AMPLIFIER: u16 = 1 << 1;
    /// Controller supports cover art (AVRCP 1.6). Without this bit the peer will not
    /// offer us an image handle, and album art never arrives.
    pub const CONTROLLER_SUPPORTS_COVER_ART: u16 = 1 << 6;
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
    /// the protocol UUID. For L2CAP the next element is the PSM. This is the lookup that
    /// finds the peer's cover-art channel, which is the one piece of the album-art path
    /// no OS stack will hand us.
    #[must_use]
    pub fn l2cap_psm(&self, attribute: u16) -> Option<u16> {
        let list = self.get(attribute)?.as_sequence()?;
        // `AdditionalProtocolDescriptorList` wraps its stacks in one more layer than
        // `ProtocolDescriptorList` does, so accept either shape rather than guessing.
        let stacks = list.iter().flat_map(|entry| match entry.as_sequence() {
            Some(inner) if inner.first().and_then(DataElement::as_uuid).is_none() => {
                inner.iter().collect::<Vec<_>>()
            }
            _ => vec![entry],
        });
        for stack in stacks {
            let parts = stack.as_sequence()?;
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
    DataElement::Sequence(vec![
        DataElement::Uuid(uuid),
        DataElement::Uint(u64::from(version)),
    ])
}

/// The common tail every record we publish shares.
fn base(handle: u32, name: &str, classes: Vec<Uuid>) -> ServiceRecord {
    ServiceRecord::new()
        .with(
            attr::SERVICE_RECORD_HANDLE,
            DataElement::Uint(u64::from(handle)),
        )
        .with(attr::SERVICE_CLASS_ID_LIST, DataElement::uuid_seq(classes))
        .with(
            attr::BROWSE_GROUP_LIST,
            DataElement::uuid_seq([Uuid::PUBLIC_BROWSE_ROOT]),
        )
        .with(
            attr::LANGUAGE_BASE_ATTRIBUTE_ID_LIST,
            // English, UTF-8, base 0x0100 — the offsets ServiceName is relative to.
            DataElement::Sequence(vec![
                DataElement::Uint(0x656e),
                DataElement::Uint(106),
                DataElement::Uint(0x0100),
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
                proto(Uuid::L2CAP, vec![DataElement::Uint(0x0019)]),
                proto(Uuid::AVDTP, vec![DataElement::Uint(0x0103)]),
            ]),
        )
        .with(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![profile(Uuid::ADVANCED_AUDIO_DISTRIBUTION, 0x0103)]),
        )
        .with(
            attr::SUPPORTED_FEATURES,
            DataElement::Uint(u64::from(a2dp_features::SPEAKER)),
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
/// [`avrcp_features::CONTROLLER_SUPPORTS_COVER_ART`] the peer will not include an image
/// handle in its metadata responses, and album art silently never arrives.
#[must_use]
pub fn avrcp_controller(handle: u32, name: &str) -> ServiceRecord {
    base(handle, name, vec![Uuid::AV_REMOTE_CONTROL_CONTROLLER])
        .with(
            attr::PROTOCOL_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![
                proto(Uuid::L2CAP, vec![DataElement::Uint(0x0017)]),
                proto(Uuid::AVCTP, vec![DataElement::Uint(0x0104)]),
            ]),
        )
        .with(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![profile(Uuid::AV_REMOTE_CONTROL, 0x0106)]),
        )
        .with(
            attr::SUPPORTED_FEATURES,
            DataElement::Uint(u64::from(
                avrcp_features::CATEGORY_1_PLAYER | avrcp_features::CONTROLLER_SUPPORTS_COVER_ART,
            )),
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
                proto(Uuid::L2CAP, vec![DataElement::Uint(0x0017)]),
                proto(Uuid::AVCTP, vec![DataElement::Uint(0x0104)]),
            ]),
        )
        .with(
            attr::BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            DataElement::Sequence(vec![profile(Uuid::AV_REMOTE_CONTROL, 0x0106)]),
        )
        .with(
            attr::SUPPORTED_FEATURES,
            DataElement::Uint(u64::from(avrcp_features::CATEGORY_2_AMPLIFIER)),
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
        assert!(tg.has_class(Uuid::AV_REMOTE_CONTROL_TARGET));
        assert!(!ct.has_class(Uuid::AV_REMOTE_CONTROL_TARGET));

        let ct_features = ct.get(attr::SUPPORTED_FEATURES).unwrap().as_uint().unwrap();
        assert_ne!(
            ct_features & u64::from(avrcp_features::CONTROLLER_SUPPORTS_COVER_ART),
            0,
            "without the cover-art bit the peer never sends an image handle"
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
            peer.l2cap_psm(attr::ADDITIONAL_PROTOCOL_DESCRIPTOR_LIST),
            Some(0x1005)
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
