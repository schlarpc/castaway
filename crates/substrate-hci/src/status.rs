//! Controller status codes (Core spec Vol 1, Part F).

use std::fmt;

/// A status byte returned by the controller in a command response or event.
///
/// Modelled as a newtype with named constants rather than an enum: the code space is
/// large, vendors add their own, and an unrecognised status must still round-trip and be
/// loggable rather than fail a parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Status(pub u8);

impl Status {
    /// The command completed.
    pub const SUCCESS: Self = Self(0x00);
    /// Unknown HCI command.
    pub const UNKNOWN_COMMAND: Self = Self(0x01);
    /// No connection exists for the given handle.
    pub const UNKNOWN_CONNECTION: Self = Self(0x02);
    /// Hardware failure.
    pub const HARDWARE_FAILURE: Self = Self(0x03);
    /// Page timed out — the remote never answered paging.
    pub const PAGE_TIMEOUT: Self = Self(0x04);
    /// Authentication failed.
    pub const AUTHENTICATION_FAILURE: Self = Self(0x05);
    /// PIN or link key missing.
    pub const PIN_OR_KEY_MISSING: Self = Self(0x06);
    /// Controller out of memory.
    pub const MEMORY_CAPACITY_EXCEEDED: Self = Self(0x07);
    /// Link supervision timeout.
    pub const CONNECTION_TIMEOUT: Self = Self(0x08);
    /// Controller is already at its connection limit.
    pub const CONNECTION_LIMIT_EXCEEDED: Self = Self(0x09);
    /// The command is disallowed in the current state.
    pub const COMMAND_DISALLOWED: Self = Self(0x0C);
    /// Connection rejected: limited resources.
    pub const REJECTED_LIMITED_RESOURCES: Self = Self(0x0D);
    /// Connection rejected: security.
    pub const REJECTED_SECURITY: Self = Self(0x0E);
    /// Connection terminated by the local host.
    pub const TERMINATED_LOCAL_HOST: Self = Self(0x16);
    /// A parameter value was invalid.
    pub const INVALID_PARAMETERS: Self = Self(0x12);
    /// The remote user ended the connection — the ordinary "phone walked away" status.
    pub const REMOTE_USER_TERMINATED: Self = Self(0x13);
    /// Pairing not allowed.
    pub const PAIRING_NOT_ALLOWED: Self = Self(0x18);
    /// The requested feature is unsupported.
    pub const UNSUPPORTED_FEATURE: Self = Self(0x11);

    /// Whether this is [`Status::SUCCESS`].
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 == 0
    }

    /// A short human-readable name, falling back to the raw code.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::SUCCESS => "success",
            Self::UNKNOWN_COMMAND => "unknown command",
            Self::UNKNOWN_CONNECTION => "unknown connection",
            Self::HARDWARE_FAILURE => "hardware failure",
            Self::PAGE_TIMEOUT => "page timeout",
            Self::AUTHENTICATION_FAILURE => "authentication failure",
            Self::PIN_OR_KEY_MISSING => "pin or key missing",
            Self::MEMORY_CAPACITY_EXCEEDED => "memory capacity exceeded",
            Self::CONNECTION_TIMEOUT => "connection timeout",
            Self::CONNECTION_LIMIT_EXCEEDED => "connection limit exceeded",
            Self::COMMAND_DISALLOWED => "command disallowed",
            Self::REJECTED_LIMITED_RESOURCES => "rejected: limited resources",
            Self::REJECTED_SECURITY => "rejected: security",
            Self::INVALID_PARAMETERS => "invalid parameters",
            Self::REMOTE_USER_TERMINATED => "remote user terminated",
            Self::TERMINATED_LOCAL_HOST => "terminated by local host",
            Self::PAIRING_NOT_ALLOWED => "pairing not allowed",
            Self::UNSUPPORTED_FEATURE => "unsupported feature",
            _ => return None,
        })
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name} ({:#04x})", self.0),
            None => write!(f, "status {:#04x}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_zero_and_named() {
        assert!(Status::SUCCESS.is_success());
        assert!(!Status::PAGE_TIMEOUT.is_success());
        assert_eq!(Status::PAGE_TIMEOUT.to_string(), "page timeout (0x04)");
    }

    #[test]
    fn an_unknown_status_still_renders() {
        // Vendors ship their own codes; an unrecognised one has to be loggable rather
        // than a parse failure.
        assert_eq!(Status(0xf3).to_string(), "status 0xf3");
    }
}
