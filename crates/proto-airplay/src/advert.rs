//! AirPlay mDNS advertisements. Two services light up Apple senders: `_airplay._tcp`
//! (mirroring/video) and `_raop._tcp` (Remote Audio Output — AirPlay audio). The TXT
//! records advertise the feature bitmask and device identity a sender inspects.

use substrate_mdns::MdnsService;

/// AirPlay's default control port.
pub const AIRPLAY_PORT: u16 = 7000;
/// RAOP (audio) default port.
pub const RAOP_PORT: u16 = 7011;

/// The `_airplay._tcp` service type.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp";
/// The `_raop._tcp` service type.
pub const RAOP_SERVICE: &str = "_raop._tcp";

/// Identity used to build the AirPlay/RAOP advertisements.
pub struct AirPlayIdentity {
    /// Friendly name shown in the AirPlay picker.
    pub name: String,
    /// The device id as a MAC-style string, e.g. `AA:BB:CC:DD:EE:FF`.
    pub device_id: String,
    /// mDNS host label (becomes `<host>.local.`).
    pub host: String,
}

impl AirPlayIdentity {
    /// The `_airplay._tcp` advertisement.
    #[must_use]
    pub fn airplay_service(&self) -> MdnsService {
        // A conservative features bitmask: video + audio + mirroring, no FairPlay-gated
        // extras we can't honor yet. `pw=false` (no password), transient pairing.
        MdnsService::new(AIRPLAY_SERVICE, &self.name, &self.host, AIRPLAY_PORT)
            .with_txt("deviceid", &self.device_id)
            .with_txt("features", "0x445F8A00,0x1C340")
            .with_txt("srcvers", "377.40.00")
            .with_txt("flags", "0x4")
            .with_txt("model", "castaway1,1")
            .with_txt("pk", "")
            .with_txt("pi", &self.device_id)
    }

    /// The `_raop._tcp` (audio) advertisement. The instance name is prefixed with the
    /// device id per RAOP convention (`<deviceid>@<name>`).
    #[must_use]
    pub fn raop_service(&self) -> MdnsService {
        let instance = format!("{}@{}", self.device_id.replace(':', ""), self.name);
        MdnsService::new(RAOP_SERVICE, instance, &self.host, RAOP_PORT)
            .with_txt("txtvers", "1")
            .with_txt("ch", "2")
            .with_txt("cn", "0,1,2,3")
            .with_txt("da", "true")
            .with_txt("et", "0,3,5")
            .with_txt("md", "0,1,2")
            .with_txt("sr", "44100")
            .with_txt("ss", "16")
            .with_txt("vs", "377.40.00")
            .with_txt("tp", "UDP")
            .with_txt("vn", "65537")
            .with_txt("am", "castaway1,1")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ident() -> AirPlayIdentity {
        AirPlayIdentity {
            name: "Hackerspace TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
        }
    }

    #[test]
    fn airplay_service_has_deviceid_and_features() {
        let s = ident().airplay_service();
        assert_eq!(s.service_type, AIRPLAY_SERVICE);
        assert_eq!(s.port, AIRPLAY_PORT);
        assert!(s.txt.iter().any(|(k, _)| k == "features"));
        assert!(s
            .txt
            .iter()
            .any(|(k, v)| k == "deviceid" && v == "AA:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn raop_instance_is_prefixed_with_deviceid() {
        let s = ident().raop_service();
        assert!(s.instance.starts_with("AABBCCDDEEFF@"));
        assert!(s.txt.iter().any(|(k, v)| k == "sr" && v == "44100"));
    }
}
