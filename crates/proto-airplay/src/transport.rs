//! `SETUP`'s `Transport` header: the three UDP ports a RAOP session runs on.
//!
//! Parsing and formatting are pure and live here; binding the sockets is the actor's
//! job. That split matters more than it looks: the ports we advertise back have to be
//! the ports actually bound, and the only way to guarantee that is for the actor to bind
//! first and hand the numbers to the state machine — never for the state machine to
//! promise a number and hope.
//!
//! A missing `control_port` or `timing_port` is fatal rather than defaultable. Those are
//! where *we* send resend requests and timing probes; guessing produces a session that
//! completes, plays, and then drifts with no way to correct, which is worse than a
//! refusal the sender can report.

use std::num::NonZeroU16;

use crate::error::TransportError;

/// The ports a sender told us to talk back on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderPorts {
    /// Where we send resend requests.
    pub control: u16,
    /// Where we send timing requests.
    pub timing: u16,
}

/// The sender-side ports one *session* has learned, each on its own schedule.
///
/// [`SenderPorts`] is what one `Transport` header names, atomically — both or refusal.
/// This is the session's accumulated knowledge, and it is two independent options
/// because the plist path genuinely learns them at different moments: the sender's
/// timing port arrives in the key-material `SETUP` (top-level `timingPort`) and its
/// control port in the type-96 stream entry (`controlPort`), possibly never. Until
/// #176 nothing read either, which is why every mirroring and `isMedia` session ran
/// with `clock_samples=0`.
///
/// `NonZeroU16` because zero on the wire means "I am not running that service"
/// (see [`parse_transport`]) — a datagram to port 0 is not a message to a sender,
/// and this makes it unrepresentable rather than checked at the send site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SenderPeers {
    /// Where we send resend requests, once the sender has said.
    pub control: Option<NonZeroU16>,
    /// Where we send timing requests, once the sender has said.
    pub timing: Option<NonZeroU16>,
}

impl From<SenderPorts> for SenderPeers {
    /// A `Transport` header names both at once; zero still means "no such service".
    fn from(ports: SenderPorts) -> Self {
        Self {
            control: NonZeroU16::new(ports.control),
            timing: NonZeroU16::new(ports.timing),
        }
    }
}

/// The ports we bound and are about to advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverPorts {
    /// Where the sender streams audio.
    pub audio: u16,
    /// Where the sender's sync packets and retransmits arrive.
    pub control: u16,
    /// Where the sender's timing replies arrive.
    pub timing: u16,
}

/// Parse the `Transport` header of a RAOP `SETUP`.
///
/// # Errors
/// [`TransportError`] if it is not a UDP record transport, or omits a port we need.
pub fn parse_transport(header: &str) -> Result<SenderPorts, TransportError> {
    let mut control = None;
    let mut timing = None;
    let mut saw_udp = false;

    for part in header.split(';').map(str::trim) {
        if part.eq_ignore_ascii_case("RTP/AVP/UDP") || part.eq_ignore_ascii_case("RTP/AVP") {
            saw_udp = true;
        } else if let Some(v) = strip_ci(part, "control_port=") {
            control = v.parse::<u16>().ok();
        } else if let Some(v) = strip_ci(part, "timing_port=") {
            timing = v.parse::<u16>().ok();
        }
    }

    if !saw_udp {
        return Err(TransportError::NotUdp);
    }
    Ok(SenderPorts {
        control: control.ok_or(TransportError::MissingPort {
            name: "control_port",
        })?,
        // Some senders legitimately omit `timing_port` and use 0 to mean "I am not
        // running a timing service" — but the ones that send it expect us to use it, and
        // a missing one with no zero is a truncated header rather than a choice.
        timing: timing.ok_or(TransportError::MissingPort {
            name: "timing_port",
        })?,
    })
}

/// Case-insensitively strip a `key=` prefix.
fn strip_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    haystack
        .get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))
        .and_then(|_| haystack.get(prefix.len()..))
}

/// Build the `Transport` header to answer a `SETUP` with.
///
/// `server_port` is where audio should be sent — a sender that gets a zero here treats
/// the session as failed, which is why the actor binds before this is ever called.
#[must_use]
pub fn format_transport(ports: ReceiverPorts) -> String {
    format!(
        "RTP/AVP/UDP;unicast;mode=record;control_port={};timing_port={};server_port={}",
        ports.control, ports.timing, ports.audio
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The header an iOS sender really sends.
    const IOS: &str = "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
        control_port=6001;timing_port=6002";

    #[test]
    fn parses_the_header_ios_sends() {
        let p = parse_transport(IOS).unwrap();
        assert_eq!(p.control, 6001);
        assert_eq!(p.timing, 6002);
    }

    #[test]
    fn header_keys_are_case_insensitive() {
        let p = parse_transport("RTP/AVP/udp;unicast;Control_Port=1;TIMING_PORT=2").unwrap();
        assert_eq!((p.control, p.timing), (1, 2));
    }

    #[test]
    fn a_missing_control_port_is_refused_rather_than_defaulted() {
        // Guessing gives a session that plays and then drifts, with no way to ask for a
        // resend and nothing in any log to say why.
        assert_eq!(
            parse_transport("RTP/AVP/UDP;unicast;mode=record;timing_port=2"),
            Err(TransportError::MissingPort {
                name: "control_port"
            })
        );
    }

    #[test]
    fn a_missing_timing_port_is_refused() {
        assert_eq!(
            parse_transport("RTP/AVP/UDP;unicast;control_port=1"),
            Err(TransportError::MissingPort {
                name: "timing_port"
            })
        );
    }

    #[test]
    fn a_non_udp_transport_is_refused() {
        assert_eq!(
            parse_transport("RTP/AVP/TCP;unicast;control_port=1;timing_port=2"),
            Err(TransportError::NotUdp)
        );
    }

    #[test]
    fn the_reply_names_all_three_ports() {
        let h = format_transport(ReceiverPorts {
            audio: 6000,
            control: 6001,
            timing: 6002,
        });
        assert!(h.contains("server_port=6000"), "{h}");
        assert!(h.contains("control_port=6001"), "{h}");
        assert!(h.contains("timing_port=6002"), "{h}");
        assert!(h.contains("mode=record"), "{h}");
    }

    #[test]
    fn a_zero_port_means_no_service_rather_than_a_peer_at_port_zero() {
        // Senders legitimately send `timing_port=0` for "I am not running a timing
        // service". A `SocketAddr` with port 0 is not a place to send datagrams, so the
        // conversion to session knowledge drops it rather than every send site checking.
        let peers = SenderPeers::from(SenderPorts {
            control: 6001,
            timing: 0,
        });
        assert_eq!(peers.control.map(NonZeroU16::get), Some(6001));
        assert_eq!(peers.timing, None);
    }

    #[test]
    fn the_reply_round_trips_through_the_parser() {
        // Our own header has to be one a sender could read, which is the cheapest check
        // that the formatter and the wire agree.
        let h = format_transport(ReceiverPorts {
            audio: 6000,
            control: 6001,
            timing: 6002,
        });
        let back = parse_transport(&h).unwrap();
        assert_eq!((back.control, back.timing), (6001, 6002));
    }
}
