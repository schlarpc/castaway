//! Who is casting, and how — as distinct from *what* they are playing.
//!
//! [`NowPlaying`] answers "what is this track"; this answers "who connected and what did
//! we negotiate". They change on completely different schedules — the device is fixed for
//! a whole session while the track changes every few minutes — so folding them together
//! would mean re-sending a phone's name with every track and losing it whenever a
//! metadata update arrived with nothing else in it.
//!
//! [`NowPlaying`]: crate::nowplaying::NowPlaying

use std::fmt;

/// What the receiver knows about the connected sender.
///
/// Every field is optional because every protocol fills in a different subset: Bluetooth
/// has a MAC and a friendly name obtained by asking, Cast has an IP and a sender name in
/// the connect message, DLNA has neither reliably.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceDescription {
    /// The sender's own name for itself — "Pixel 8", "Chaz's iPhone".
    pub display_name: Option<String>,
    /// A stable identifier as text: a Bluetooth address, an IP, a Cast sender id.
    pub address: Option<String>,
    /// What was negotiated to carry the media — "aptX HD · 48 kHz · stereo".
    pub link: Option<String>,
}

impl SourceDescription {
    /// An empty description.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style name setter.
    #[must_use]
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Builder-style address setter.
    #[must_use]
    pub fn with_address(mut self, address: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Builder-style link-description setter.
    #[must_use]
    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }

    /// Merge `other` over this one, keeping what it does not specify.
    ///
    /// Descriptions arrive in pieces and out of order — Bluetooth learns the address when
    /// the link comes up, the codec at configuration, and the friendly name whenever the
    /// remote-name request happens to complete. A later update that knows only the codec
    /// must not erase the name.
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        if other.display_name.is_some() {
            self.display_name = other.display_name;
        }
        if other.address.is_some() {
            self.address = other.address;
        }
        if other.link.is_some() {
            self.link = other.link;
        }
        self
    }

    /// The best name available for a person to read.
    ///
    /// Falls back to the address, because "AA:BB:CC:DD:EE:FF" on screen is far more use
    /// than "Unknown device" when working out which phone in the room is connected.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.display_name.as_deref().or(self.address.as_deref())
    }

    /// Whether anything at all is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.display_name.is_none() && self.address.is_none() && self.link.is_none()
    }
}

impl fmt::Display for SourceDescription {
    /// One line suitable for an OSD banner: `Pixel 8 (AA:BB:…) · aptX HD · 48 kHz`.
    ///
    /// The address is shown *alongside* the name rather than instead of it, because two
    /// phones in a room are routinely both called "iPhone".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut wrote = false;
        if let Some(name) = &self.display_name {
            f.write_str(name)?;
            wrote = true;
        }
        if let Some(address) = &self.address {
            if wrote {
                write!(f, " ({address})")?;
            } else {
                f.write_str(address)?;
            }
            wrote = true;
        }
        if let Some(link) = &self.link {
            if wrote {
                f.write_str(" · ")?;
            }
            f.write_str(link)?;
            wrote = true;
        }
        if !wrote {
            f.write_str("unknown device")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_update_does_not_erase_what_it_does_not_know() {
        // The failure this exists to prevent: Bluetooth learns the address at link-up,
        // the codec at configuration, and the name whenever the remote-name request
        // finishes. Each arrives knowing only its own field.
        let base = SourceDescription::new()
            .with_address("AA:BB:CC:DD:EE:FF")
            .with_display_name("Pixel 8");
        let codec_only = SourceDescription::new().with_link("aptX HD · 48 kHz · stereo");

        let merged = base.merged(codec_only);
        assert_eq!(merged.display_name.as_deref(), Some("Pixel 8"));
        assert_eq!(merged.address.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(merged.link.as_deref(), Some("aptX HD · 48 kHz · stereo"));
    }

    #[test]
    fn a_name_arriving_late_replaces_nothing_else() {
        let base = SourceDescription::new()
            .with_address("AA:BB:CC:DD:EE:FF")
            .with_link("SBC · 44.1 kHz · joint stereo");
        let named = base.merged(SourceDescription::new().with_display_name("Chaz's iPhone"));
        assert_eq!(named.display_name.as_deref(), Some("Chaz's iPhone"));
        assert_eq!(named.link.as_deref(), Some("SBC · 44.1 kHz · joint stereo"));
    }

    #[test]
    fn the_address_is_shown_alongside_the_name_not_instead_of_it() {
        // Two phones in a room are routinely both called "iPhone".
        let full = SourceDescription::new()
            .with_display_name("Pixel 8")
            .with_address("AA:BB:CC:DD:EE:FF")
            .with_link("aptX HD · 48 kHz");
        assert_eq!(
            full.to_string(),
            "Pixel 8 (AA:BB:CC:DD:EE:FF) · aptX HD · 48 kHz"
        );
    }

    #[test]
    fn an_unnamed_device_still_shows_its_address() {
        // Plenty of senders never answer a name request. A MAC on screen beats
        // "Unknown device" when working out which phone is connected.
        let anon = SourceDescription::new().with_address("AA:BB:CC:DD:EE:FF");
        assert_eq!(anon.label(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(anon.to_string(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn a_description_that_knows_nothing_says_so() {
        let empty = SourceDescription::new();
        assert!(empty.is_empty());
        assert_eq!(empty.label(), None);
        assert_eq!(empty.to_string(), "unknown device");
    }

    #[test]
    fn the_name_is_preferred_over_the_address_as_a_label() {
        let named = SourceDescription::new()
            .with_display_name("Pixel 8")
            .with_address("AA:BB:CC:DD:EE:FF");
        assert_eq!(named.label(), Some("Pixel 8"));
    }
}
