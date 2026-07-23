//! On-screen-display overlays, as a **multi-producer channel** decoupled from the media
//! pipeline. Any component — the session manager ("Now casting from …"), a protocol
//! adapter ("Buffering…", "Volume 50%"), or the app itself ("no senders yet") — holds a
//! cloneable [`OsdSink`] and posts. Exactly one consumer holds the [`OsdReceiver`]: the
//! render backend (which rasterizes banners) or, headless, a log drain.
//!
//! This lives in `core` (not `pipeline`) precisely so sources that don't depend on the
//! GPU/render crate can still inject messages — the same reasoning as [`crate::SessionSink`].

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// A message to show on the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsdMessage {
    /// The text to show.
    pub text: String,
    /// How long to show it; `None` means until replaced or cleared.
    pub ttl: Option<Duration>,
}

impl OsdMessage {
    /// A transient banner shown for `ttl`, then auto-cleared.
    #[must_use]
    pub fn banner(text: impl Into<String>, ttl: Duration) -> Self {
        Self {
            text: text.into(),
            ttl: Some(ttl),
        }
    }

    /// A message shown until explicitly replaced or cleared.
    #[must_use]
    pub fn sticky(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ttl: None,
        }
    }
}

/// A command on the OSD channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsdCommand {
    /// Show (replacing any current message).
    Show(OsdMessage),
    /// Clear the overlay.
    Clear,
}

/// A cloneable handle for posting OSD messages. Cheap to clone and `Send`, so hand a
/// clone to every source that should be able to speak on the overlay.
#[derive(Debug, Clone)]
pub struct OsdSink {
    tx: Sender<OsdCommand>,
}

impl OsdSink {
    /// Show a message. Silently no-ops if the consumer has gone away.
    pub fn show(&self, message: OsdMessage) {
        let _ = self.tx.send(OsdCommand::Show(message));
    }

    /// Show a transient banner for `ttl` (convenience over [`Self::show`]).
    pub fn banner(&self, text: impl Into<String>, ttl: Duration) {
        self.show(OsdMessage::banner(text, ttl));
    }

    /// Clear the overlay.
    pub fn clear(&self) {
        let _ = self.tx.send(OsdCommand::Clear);
    }
}

/// The single consumer end of the OSD channel.
pub struct OsdReceiver {
    rx: Receiver<OsdCommand>,
}

impl OsdReceiver {
    /// Non-blocking receive (for the render loop, polled each frame).
    #[must_use]
    pub fn try_recv(&self) -> Option<OsdCommand> {
        self.rx.try_recv().ok()
    }

    /// Blocking receive (for a headless log-drain). Returns `None` once all sinks drop.
    #[must_use]
    pub fn recv(&self) -> Option<OsdCommand> {
        self.rx.recv().ok()
    }
}

/// Create an OSD channel: a cloneable [`OsdSink`] for producers and one [`OsdReceiver`]
/// for the consumer.
#[must_use]
pub fn osd_channel() -> (OsdSink, OsdReceiver) {
    let (tx, rx) = std::sync::mpsc::channel();
    (OsdSink { tx }, OsdReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_producers_land_at_the_one_consumer() {
        let (sink, rx) = osd_channel();
        let a = sink.clone();
        let b = sink.clone();
        a.banner("from A", Duration::from_secs(2));
        b.show(OsdMessage::sticky("from B"));
        sink.clear();

        assert_eq!(
            rx.try_recv(),
            Some(OsdCommand::Show(OsdMessage::banner(
                "from A",
                Duration::from_secs(2)
            )))
        );
        assert_eq!(
            rx.try_recv(),
            Some(OsdCommand::Show(OsdMessage::sticky("from B")))
        );
        assert_eq!(rx.try_recv(), Some(OsdCommand::Clear));
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn posting_after_consumer_drops_is_silent() {
        let (sink, rx) = osd_channel();
        drop(rx);
        sink.banner("nobody listening", Duration::from_secs(1)); // must not panic
    }
}
