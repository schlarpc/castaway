//! The wpa_supplicant control interface — how a Linux box becomes a Wi-Fi Direct group
//! owner without this crate touching a driver.
//!
//! This is *not* the Miracast wire protocol. It is the local IPC that gets us to the
//! point where the wire protocol has a socket to run on, and it is the Linux half of the
//! [`castaway_core::MiracastBackend`] seam (ground rule 5): Windows reaches the same
//! point through WinRT and shares none of this.
//!
//! The control interface is a Unix *datagram* socket carrying plain text. Commands get a
//! reply (`OK`, `FAIL`, or data); after `ATTACH`, the same socket also delivers unsolicited
//! events prefixed with a priority in angle brackets. Both directions are parsed here,
//! synchronously and without a socket, so the sequencing that brings a group up is
//! testable against captured strings — which matters more than usual, because the real
//! thing needs a radio, a driver with P2P support, and root.
//!
//! What this deliberately does not do is *decide*. It parses and it formats; the actor
//! decides. That keeps the one piece of this crate that cannot be tested on CI down to
//! the socket read/write itself.

use std::fmt;

/// The interface name pattern wpa_supplicant gives a P2P group. The group interface is
/// not the one we configured — it is created when the group starts, and the RTSP listener
/// has to bind to *it*, not to the parent.
pub const GROUP_IFACE_PREFIX: &str = "p2p-";

/// A command sent to wpa_supplicant.
///
/// An enum rather than strings because the failure mode of a typo'd control command is
/// silence: wpa_supplicant answers `FAIL` and a sink that ignored it advertises nothing,
/// forever, with a running process and an empty log.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WpaCommand {
    /// Liveness check. The only command that answers something other than `OK`/`FAIL`.
    Ping,
    /// Subscribe this socket to unsolicited events.
    Attach,
    /// Set a global or per-interface variable.
    Set {
        /// The variable name.
        key: &'static str,
        /// Its value.
        value: String,
    },
    /// Set one WFD information-element subelement, or clear it with an empty body.
    ///
    /// wpa_supplicant prepends the subelement id itself, so the hex payload starts with
    /// the subelement's own 2-byte length — see [`crate::ie`].
    WfdSubelemSet {
        /// Subelement id.
        id: u8,
        /// Length-prefixed subelement body, hex-encoded.
        hex: String,
    },
    /// Become the group owner of an autonomous P2P group on `freq`, persistent or not.
    P2pGroupAdd {
        /// Operating frequency in MHz. `None` lets wpa_supplicant choose.
        freq: Option<u16>,
    },
    /// Tear the group down.
    P2pGroupRemove {
        /// The group interface name.
        iface: String,
    },
    /// Accept a push-button enrolment from a peer that asked for one.
    WpsPbc {
        /// The peer's device address, or `None` for any.
        peer: Option<MacAddr>,
    },
    /// Answer a provision-discovery request so the peer may proceed to WPS.
    P2pProvDisc {
        /// The peer's device address.
        peer: MacAddr,
        /// The config method to answer with.
        method: ProvisionMethod,
    },
    /// Start or stop responding to P2P discovery.
    P2pFind,
    /// Stop responding to P2P discovery.
    P2pStopFind,
}

impl fmt::Display for WpaCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ping => f.write_str("PING"),
            Self::Attach => f.write_str("ATTACH"),
            Self::Set { key, value } => write!(f, "SET {key} {value}"),
            Self::WfdSubelemSet { id, hex } => write!(f, "WFD_SUBELEM_SET {id} {hex}"),
            Self::P2pGroupAdd { freq } => match freq {
                Some(mhz) => write!(f, "P2P_GROUP_ADD freq={mhz}"),
                None => f.write_str("P2P_GROUP_ADD"),
            },
            Self::P2pGroupRemove { iface } => write!(f, "P2P_GROUP_REMOVE {iface}"),
            Self::WpsPbc { peer } => match peer {
                Some(mac) => write!(f, "WPS_PBC {mac}"),
                None => f.write_str("WPS_PBC any"),
            },
            Self::P2pProvDisc { peer, method } => write!(f, "P2P_PROV_DISC {peer} {method}"),
            Self::P2pFind => f.write_str("P2P_FIND"),
            Self::P2pStopFind => f.write_str("P2P_STOP_FIND"),
        }
    }
}

/// A WPS config method, as the control interface names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProvisionMethod {
    /// Push-button. The only method a sink with no keypad and no label can offer, and
    /// what every Miracast sender uses.
    Pbc,
    /// A PIN the peer displays and we enter.
    Display,
    /// A PIN we display and the peer enters.
    Keypad,
}

impl fmt::Display for ProvisionMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pbc => "pbc",
            Self::Display => "display",
            Self::Keypad => "keypad",
        })
    }
}

/// A 48-bit device address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// The address's six bytes.
    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    /// Parse the `aa:bb:cc:dd:ee:ff` form the control interface prints.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut octets = [0u8; 6];
        let mut parts = s.split(':');
        for slot in &mut octets {
            *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self(octets))
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// An unsolicited event from wpa_supplicant.
///
/// Only the ones that change what the sink must do next are modelled. Everything else is
/// [`WpaEvent::Other`] rather than an error: the control interface emits scan results,
/// regulatory changes and driver chatter continuously, and a backend that treated an
/// unknown line as a failure would fall over on a kernel upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WpaEvent {
    /// A peer is asking to negotiate a group. We are already the GO, so this is the
    /// signal to authorize rather than to negotiate.
    GoNegRequest {
        /// The peer's device address.
        peer: MacAddr,
    },
    /// A peer pressed the (virtual) button and wants push-button enrolment.
    ProvDiscPbcRequest {
        /// The peer's device address.
        peer: MacAddr,
    },
    /// The group is up. Its interface — *not* the one we configured — is where the RTSP
    /// listener and the media sockets have to bind.
    GroupStarted {
        /// The newly created group interface.
        iface: String,
        /// Whether we are the group owner. A sink that finds itself a client has lost a
        /// negotiation it should not have been in.
        group_owner: bool,
    },
    /// The group is gone; every socket bound to its interface is now invalid.
    GroupRemoved {
        /// The group interface that went away.
        iface: String,
    },
    /// A peer associated with our group. The RTSP session starts after this.
    StationConnected {
        /// The peer's address.
        peer: MacAddr,
    },
    /// A peer left.
    StationDisconnected {
        /// The peer's address.
        peer: MacAddr,
    },
    /// A peer appeared in discovery, with the WFD device information it advertised.
    DeviceFound {
        /// The peer's device address.
        peer: MacAddr,
        /// The friendly name it published.
        name: Option<String>,
        /// The raw `wfd_dev_info` hex, if it published a WFD IE. Decoding is
        /// [`crate::ie`]'s job; keeping the bytes means a peer we cannot parse still
        /// shows up in a log with something actionable in it.
        wfd_dev_info: Option<String>,
    },
    /// Enrolment succeeded.
    WpsSuccess,
    /// Enrolment failed; the peer will usually retry.
    WpsFailed,
    /// Something else. Kept whole for logging.
    Other(String),
}

/// Strip wpa_supplicant's `<3>` priority prefix, if present.
fn strip_priority(line: &str) -> &str {
    line.strip_prefix('<')
        .and_then(|rest| rest.split_once('>'))
        .map_or(line, |(_, rest)| rest)
}

/// Pull a `key=value` field out of an event's trailing key-value list.
///
/// Values may be single-quoted (`name='Living Room'`) or bare. Double quotes appear on
/// `ssid=` and are handled the same way.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.match_indices(key).find_map(|(i, _)| {
        let before_ok = i == 0 || line.as_bytes().get(i.wrapping_sub(1)) == Some(&b' ');
        let after = line.get(i + key.len()..)?;
        (before_ok && after.starts_with('=')).then(|| i + key.len() + 1)
    })?;
    let rest = line.get(at..)?;
    for quote in ['\'', '"'] {
        if let Some(inner) = rest.strip_prefix(quote) {
            return inner.split(quote).next();
        }
    }
    Some(rest.split(' ').next().unwrap_or(rest))
}

impl WpaEvent {
    /// Parse one line from the control socket.
    #[must_use]
    pub fn parse(line: &str) -> Self {
        let body = strip_priority(line.trim_end());
        let (tag, rest) = body.split_once(' ').unwrap_or((body, ""));
        let first = rest.split(' ').next().unwrap_or("");
        match tag {
            "P2P-GO-NEG-REQUEST" => MacAddr::parse(first).map_or_else(
                || Self::Other(body.to_owned()),
                |peer| Self::GoNegRequest { peer },
            ),
            "P2P-PROV-DISC-PBC-REQ" => MacAddr::parse(first).map_or_else(
                || Self::Other(body.to_owned()),
                |peer| Self::ProvDiscPbcRequest { peer },
            ),
            "P2P-GROUP-STARTED" => Self::GroupStarted {
                iface: first.to_owned(),
                // The role is a bare word in the second position: `GO` or `client`.
                group_owner: rest.split(' ').nth(1) == Some("GO"),
            },
            "P2P-GROUP-REMOVED" => Self::GroupRemoved {
                iface: first.to_owned(),
            },
            "AP-STA-CONNECTED" => MacAddr::parse(first).map_or_else(
                || Self::Other(body.to_owned()),
                |peer| Self::StationConnected { peer },
            ),
            "AP-STA-DISCONNECTED" => MacAddr::parse(first).map_or_else(
                || Self::Other(body.to_owned()),
                |peer| Self::StationDisconnected { peer },
            ),
            "P2P-DEVICE-FOUND" => MacAddr::parse(first).map_or_else(
                || Self::Other(body.to_owned()),
                |peer| Self::DeviceFound {
                    peer,
                    name: field(rest, "name").map(ToOwned::to_owned),
                    wfd_dev_info: field(rest, "wfd_dev_info").map(ToOwned::to_owned),
                },
            ),
            "WPS-SUCCESS" | "WPS-REG-SUCCESS" => Self::WpsSuccess,
            "WPS-FAIL" | "WPS-TIMEOUT" => Self::WpsFailed,
            _ => Self::Other(body.to_owned()),
        }
    }

    /// Whether this line is an event at all, as opposed to a command reply.
    ///
    /// wpa_supplicant multiplexes both onto one socket and distinguishes them only by the
    /// priority prefix, so this is the whole demultiplexer.
    #[must_use]
    pub fn is_event_line(line: &str) -> bool {
        line.starts_with('<')
    }
}

/// A command's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WpaReply {
    /// `OK`.
    Ok,
    /// `FAIL`, or `UNKNOWN COMMAND`.
    Fail(String),
    /// Anything else — `PONG`, a group interface name, a variable's value.
    Data(String),
}

impl WpaReply {
    /// Classify a reply line.
    #[must_use]
    pub fn parse(reply: &str) -> Self {
        let trimmed = reply.trim_end_matches(['\n', '\r']);
        match trimmed {
            "OK" => Self::Ok,
            "FAIL" | "UNKNOWN COMMAND" => Self::Fail(trimmed.to_owned()),
            other if other.starts_with("FAIL") => Self::Fail(other.to_owned()),
            other => Self::Data(other.to_owned()),
        }
    }

    /// The reply as a `Result`, so a caller can `?` on a command that must have worked.
    ///
    /// # Errors
    /// [`crate::MiracastError::Backend`] with the command and the reply, because a bare
    /// "FAIL" in a log is unactionable — every one of these commands can fail, and which
    /// one did is the whole diagnosis.
    pub fn require_ok(self, command: &WpaCommand) -> Result<(), crate::MiracastError> {
        match self {
            Self::Ok => Ok(()),
            Self::Fail(reason) | Self::Data(reason) => Err(crate::MiracastError::Backend(format!(
                "`{command}` answered `{reason}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_group_started_event_names_the_interface_to_bind_to() {
        // The group interface is created by the event; binding to the parent silently
        // listens on the wrong radio path and the sender's SETUP times out.
        let e = WpaEvent::parse(
            "<3>P2P-GROUP-STARTED p2p-wlan0-0 GO ssid=\"DIRECT-Ab\" freq=5180 \
             passphrase=\"hunter2xy\" go_dev_addr=02:11:22:33:44:55",
        );
        assert_eq!(
            e,
            WpaEvent::GroupStarted {
                iface: "p2p-wlan0-0".to_owned(),
                group_owner: true,
            }
        );
    }

    #[test]
    fn finding_ourselves_a_client_is_visible_rather_than_assumed() {
        let e = WpaEvent::parse("<3>P2P-GROUP-STARTED p2p-wlan0-1 client ssid=\"DIRECT-zz\"");
        assert_eq!(
            e,
            WpaEvent::GroupStarted {
                iface: "p2p-wlan0-1".to_owned(),
                group_owner: false,
            }
        );
    }

    #[test]
    fn a_device_found_event_carries_the_name_and_the_wfd_ie() {
        let e = WpaEvent::parse(
            "<3>P2P-DEVICE-FOUND 02:aa:bb:cc:dd:ee p2p_dev_addr=02:aa:bb:cc:dd:ee \
             pri_dev_type=10-0050F204-5 name='Chaz Phone' config_methods=0x188 \
             dev_capab=0x25 group_capab=0x0 wfd_dev_info=0x00111c440032 new=1",
        );
        assert_eq!(
            e,
            WpaEvent::DeviceFound {
                peer: MacAddr::parse("02:aa:bb:cc:dd:ee").unwrap(),
                name: Some("Chaz Phone".to_owned()),
                wfd_dev_info: Some("0x00111c440032".to_owned()),
            }
        );
    }

    #[test]
    fn a_quoted_name_with_spaces_survives_the_split() {
        // Splitting on whitespace truncates every two-word device name, and the symptom
        // is a peer list where half the phones in the room are called "Chaz".
        let e =
            WpaEvent::parse("<3>P2P-DEVICE-FOUND 02:aa:bb:cc:dd:ee name='Living Room TV' new=1");
        match e {
            WpaEvent::DeviceFound { name, .. } => {
                assert_eq!(name.as_deref(), Some("Living Room TV"))
            }
            other => panic!("expected a device-found event, got {other:?}"),
        }
    }

    #[test]
    fn a_field_name_that_is_a_suffix_of_another_is_not_matched() {
        // `dev_info=` must not satisfy a lookup for `wfd_dev_info`, and vice versa.
        let line = "P2P-DEVICE-FOUND 02:aa:bb:cc:dd:ee dev_info=0x1 new=1";
        assert_eq!(field(line, "wfd_dev_info"), None);
        assert_eq!(field(line, "dev_info"), Some("0x1"));
    }

    #[test]
    fn station_events_carry_the_peer() {
        assert_eq!(
            WpaEvent::parse("<3>AP-STA-CONNECTED 02:11:22:33:44:55"),
            WpaEvent::StationConnected {
                peer: MacAddr::parse("02:11:22:33:44:55").unwrap()
            }
        );
        assert_eq!(
            WpaEvent::parse("<3>AP-STA-DISCONNECTED 02:11:22:33:44:55"),
            WpaEvent::StationDisconnected {
                peer: MacAddr::parse("02:11:22:33:44:55").unwrap()
            }
        );
    }

    #[test]
    fn an_unknown_event_is_kept_whole_rather_than_failing() {
        // The control interface emits driver chatter continuously; treating an unknown
        // line as an error means falling over on a kernel upgrade.
        let e = WpaEvent::parse("<3>CTRL-EVENT-REGDOM-CHANGE init=CORE type=WORLD");
        assert_eq!(
            e,
            WpaEvent::Other("CTRL-EVENT-REGDOM-CHANGE init=CORE type=WORLD".to_owned())
        );
    }

    #[test]
    fn events_and_replies_are_told_apart_by_the_priority_prefix() {
        assert!(WpaEvent::is_event_line(
            "<3>AP-STA-CONNECTED 02:11:22:33:44:55"
        ));
        assert!(!WpaEvent::is_event_line("OK"));
        assert!(!WpaEvent::is_event_line("p2p-wlan0-0"));
    }

    #[test]
    fn commands_format_as_the_control_interface_expects() {
        assert_eq!(WpaCommand::Ping.to_string(), "PING");
        assert_eq!(
            WpaCommand::Set {
                key: "wifi_display",
                value: "1".to_owned()
            }
            .to_string(),
            "SET wifi_display 1"
        );
        assert_eq!(
            WpaCommand::WfdSubelemSet {
                id: 0,
                hex: "000600411c440036".to_owned()
            }
            .to_string(),
            "WFD_SUBELEM_SET 0 000600411c440036"
        );
        assert_eq!(
            WpaCommand::P2pGroupAdd { freq: Some(5180) }.to_string(),
            "P2P_GROUP_ADD freq=5180"
        );
        assert_eq!(WpaCommand::WpsPbc { peer: None }.to_string(), "WPS_PBC any");
    }

    #[test]
    fn a_failed_command_names_itself_in_the_error() {
        let cmd = WpaCommand::P2pGroupAdd { freq: Some(2412) };
        let err = WpaReply::parse("FAIL").require_ok(&cmd).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("P2P_GROUP_ADD freq=2412"), "{msg}");
        assert!(msg.contains("FAIL"), "{msg}");
    }

    #[test]
    fn ok_and_data_replies_are_distinguished() {
        assert_eq!(WpaReply::parse("OK\n"), WpaReply::Ok);
        assert_eq!(WpaReply::parse("PONG\n"), WpaReply::Data("PONG".to_owned()));
        assert!(matches!(WpaReply::parse("FAIL-BUSY"), WpaReply::Fail(_)));
    }

    #[test]
    fn mac_addresses_round_trip() {
        let mac = MacAddr::parse("02:0a:0b:0c:0d:0e").unwrap();
        assert_eq!(mac.to_string(), "02:0a:0b:0c:0d:0e");
        assert_eq!(mac.octets(), [0x02, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e]);
        assert!(MacAddr::parse("02:0a:0b:0c:0d").is_none());
        assert!(MacAddr::parse("02:0a:0b:0c:0d:0e:0f").is_none());
    }
}
