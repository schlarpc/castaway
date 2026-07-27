//! The reverse channel: how the *receiver* drives the *sender*.
//!
//! Every adapter before Bluetooth was one-directional — it emitted [`SessionEvent`]s and
//! never heard back. A2DP/AVRCP is the first source where the panel can talk back: the
//! C6522QT is a touch screen, so a finger on "pause" has to reach the phone that is
//! actually playing. That is a capability of a live session, not another event, so it
//! lives here as a handle the adapter publishes.
//!
//! [`SessionEvent`]: crate::event::SessionEvent
//!
//! The correctness idea is [`ControlCapabilities`]: a peer advertises which verbs it
//! honours (AVRCP hands us a supported-features bitmask during connection setup), and
//! [`RemoteControl::issue`] refuses anything outside that set *before* it reaches the
//! wire. A UI built against `capabilities()` cannot render a button the phone will
//! reject, and an adapter cannot forget to check (ground rule 1).

use std::fmt;
use std::ops::{BitOr, BitOrAssign};

use crate::error::CoreError;
use crate::event::ControlTxn;

/// The set of transport verbs a peer has advertised support for.
///
/// A bit set rather than a `Vec<ControlTxn>` because it is a *capability*, not a
/// sequence: order and duplicates are meaningless, and membership is the only query.
/// Construct by OR-ing the associated constants.
///
/// ```
/// # use castaway_core::{ControlCapabilities, ControlTxn};
/// let caps = ControlCapabilities::PLAY | ControlCapabilities::PAUSE;
/// assert!(caps.supports(&ControlTxn::Play));
/// assert!(!caps.supports(&ControlTxn::Next));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControlCapabilities(u16);

impl ControlCapabilities {
    /// No control at all — the session is playback-only.
    pub const NONE: Self = Self(0);

    /// The raw bits, for storing the set somewhere atomic.
    ///
    /// A capability set is not fixed for the life of a session: what a peer supports may
    /// only be learned after the surface has been published — an AVRCP peer's feature
    /// bitmask arrives over SDP, which completes after AVCTP does.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Rebuild from [`Self::bits`].
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }
    /// Resume playback ([`ControlTxn::Play`]).
    pub const PLAY: Self = Self(1 << 0);
    /// Pause playback ([`ControlTxn::Pause`]).
    pub const PAUSE: Self = Self(1 << 1);
    /// Stop and tear down the current item ([`ControlTxn::Stop`]).
    pub const STOP: Self = Self(1 << 2);
    /// Seek to an absolute position ([`ControlTxn::Seek`]).
    pub const SEEK: Self = Self(1 << 3);
    /// Set output volume ([`ControlTxn::Volume`]).
    pub const VOLUME: Self = Self(1 << 4);
    /// Mute/unmute ([`ControlTxn::Mute`]).
    pub const MUTE: Self = Self(1 << 5);
    /// Skip forward ([`ControlTxn::Next`]).
    pub const NEXT: Self = Self(1 << 6);
    /// Skip backward ([`ControlTxn::Previous`]).
    pub const PREVIOUS: Self = Self(1 << 7);
    /// Replace the play queue ([`ControlTxn::SetQueue`]).
    pub const SET_QUEUE: Self = Self(1 << 8);
    /// Turn shuffle on or off ([`ControlTxn::Shuffle`]).
    pub const SHUFFLE: Self = Self(1 << 9);
    /// Set the repeat mode ([`ControlTxn::Repeat`]).
    pub const REPEAT: Self = Self(1 << 10);

    /// The four transport verbs every AVRCP peer worth the name implements.
    pub const TRANSPORT: Self =
        Self(Self::PLAY.0 | Self::PAUSE.0 | Self::NEXT.0 | Self::PREVIOUS.0);

    /// Union of two sets, usable in `const` context.
    ///
    /// [`BitOr`] is the idiomatic spelling and stays the one to reach for, but it cannot
    /// be called from a `const fn`, which is exactly where an adapter wants to state its
    /// fixed capability set once.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every capability in `other` is present in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether this peer honours `txn`.
    ///
    /// The mapping is exhaustive on purpose: a new [`ControlTxn`] variant fails to
    /// compile here until someone decides which capability bit gates it.
    #[must_use]
    pub const fn supports(self, txn: &ControlTxn) -> bool {
        self.contains(Self::required_for(txn))
    }

    /// The single capability bit a transaction requires.
    #[must_use]
    pub const fn required_for(txn: &ControlTxn) -> Self {
        match txn {
            ControlTxn::Play => Self::PLAY,
            ControlTxn::Pause => Self::PAUSE,
            ControlTxn::Stop => Self::STOP,
            ControlTxn::Seek(_) => Self::SEEK,
            ControlTxn::Volume(_) => Self::VOLUME,
            ControlTxn::Mute(_) => Self::MUTE,
            ControlTxn::Next => Self::NEXT,
            ControlTxn::Previous => Self::PREVIOUS,
            ControlTxn::SetQueue { .. } => Self::SET_QUEUE,
            ControlTxn::Shuffle(_) => Self::SHUFFLE,
            ControlTxn::Repeat(_) => Self::REPEAT,
        }
    }

    /// Whether no capability at all is advertised.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for ControlCapabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ControlCapabilities {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A handle back into a live session, so the receiver can drive the sender.
///
/// Published by an adapter via [`SessionEvent::ControlSurface`] once the peer's control
/// channel is actually up — which is genuinely later than the media stream for
/// Bluetooth, where AVCTP is a second L2CAP channel that may connect after audio is
/// already flowing. Adapters that can't drive their sender simply never publish one.
///
/// [`SessionEvent::ControlSurface`]: crate::event::SessionEvent::ControlSurface
#[async_trait::async_trait]
pub trait RemoteControl: Send + Sync + fmt::Debug {
    /// Which verbs the peer advertised. Callers should drive their UI from this.
    fn capabilities(&self) -> ControlCapabilities;

    /// Send a transaction the caller has already established is supported.
    ///
    /// Implementors write this one; callers should use [`RemoteControl::issue`], which
    /// applies the capability check first.
    ///
    /// # Errors
    /// Adapter-specific failure (channel dropped, peer rejected), as [`CoreError::Adapter`].
    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError>;

    /// Send a transaction to the peer, refusing anything it never advertised.
    ///
    /// # Errors
    /// [`CoreError::UnsupportedControl`] if the peer doesn't honour `txn`; otherwise
    /// whatever [`RemoteControl::issue_unchecked`] returns.
    async fn issue(&self, txn: ControlTxn) -> Result<(), CoreError> {
        if !self.capabilities().supports(&txn) {
            return Err(CoreError::UnsupportedControl(format!("{txn:?}")));
        }
        self.issue_unchecked(txn).await
    }

    /// The media this source handed to the pipeline has ended, or failed to play.
    ///
    /// Defaulted to nothing, and most sources should leave it that way: for Bluetooth and
    /// Spotify the *phone* is the player, it knows perfectly well when a track ended, and
    /// telling it would be telling it something it told us.
    ///
    /// It matters for the sources where the receiver is the player — a DLNA control point
    /// that pushed a URL, a Cast sender that sent `LOAD`. Those protocols oblige us to
    /// report the transport state, and without this the answer stayed `PLAYING` for a URL
    /// the box could not even fetch: the phone showed a healthy session over a blank
    /// panel, and a queued playlist waiting for the item to end waited forever.
    ///
    /// Best-effort by contract. A source that has already gone gets its error logged and
    /// dropped, because the session is ending either way.
    ///
    /// # Errors
    /// Adapter-specific failure, as [`CoreError::Adapter`].
    async fn media_ended(&self, end: crate::playback::PlaybackEnd) -> Result<(), CoreError> {
        let _ = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;

    #[derive(Debug)]
    struct FakeRemote {
        caps: ControlCapabilities,
        sent: Mutex<Vec<ControlTxn>>,
    }

    #[async_trait::async_trait]
    impl RemoteControl for FakeRemote {
        fn capabilities(&self) -> ControlCapabilities {
            self.caps
        }
        async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
            self.sent.lock().expect("poisoned").push(txn);
            Ok(())
        }
    }

    #[test]
    fn transport_bundle_is_the_four_common_verbs() {
        let t = ControlCapabilities::TRANSPORT;
        assert!(t.supports(&ControlTxn::Play));
        assert!(t.supports(&ControlTxn::Pause));
        assert!(t.supports(&ControlTxn::Next));
        assert!(t.supports(&ControlTxn::Previous));
        // Seek is *not* in the bundle: plenty of AVRCP peers advertise transport but
        // refuse absolute position, and guessing otherwise is a rejected command.
        assert!(!t.supports(&ControlTxn::Seek(Duration::from_secs(1))));
    }

    #[test]
    fn empty_capabilities_support_nothing() {
        let none = ControlCapabilities::NONE;
        assert!(none.is_empty());
        assert!(!none.supports(&ControlTxn::Play));
        assert!(!none.supports(&ControlTxn::Volume(0.5)));
    }

    #[tokio::test]
    async fn issue_refuses_a_verb_the_peer_never_advertised() {
        // The whole point of the capability set: a UI that offers "next" to a peer with
        // play/pause only gets a typed refusal instead of a command the phone drops on
        // the floor, which is indistinguishable from a hung session.
        let remote = FakeRemote {
            caps: ControlCapabilities::PLAY | ControlCapabilities::PAUSE,
            sent: Mutex::new(Vec::new()),
        };
        assert!(matches!(
            remote.issue(ControlTxn::Next).await,
            Err(CoreError::UnsupportedControl(_))
        ));
        assert!(
            remote.sent.lock().unwrap().is_empty(),
            "nothing hit the wire"
        );

        remote.issue(ControlTxn::Pause).await.unwrap();
        assert_eq!(&*remote.sent.lock().unwrap(), &[ControlTxn::Pause]);
    }
}
